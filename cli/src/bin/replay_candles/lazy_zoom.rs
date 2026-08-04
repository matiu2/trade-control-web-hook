//! Two-pass **lazy** sub-bar zoom: fetch the finer series only for the bars
//! that actually need it.
//!
//! # Why
//!
//! The eager zoom pulls a finer series (M1 under an H1 plan) across the WHOLE
//! coarse window before the sim runs. But [`super::fill_sim::SubBars`] is
//! consulted only when a post-fill bar straddles BOTH the stop-loss and the
//! take-profit, and the exit loop `return`s on the first such bar — so each
//! simulated entry zooms **at most once**. Measured on the CAD/SGD H1 fixture:
//! 11,160 M1 slots pulled to disambiguate at most 2 bars (120 M1 slots), and
//! cells with no entry at all pull the same 11k and consume none of it.
//!
//! That waste compounds with a candle-cache property: a partial broker response
//! leaves non-ticking minutes unrecorded, so an illiquid cross re-fetches the
//! same scattered holes on every run (CAD/SGD 2026-07-22: 200 missing of 1440).
//! Fetching a narrow window sidesteps that without ever recording "don't ask
//! again" — negative caching is deliberately NOT the fix here, because a
//! misclassified permanent error previously left holes that were never retried.
//!
//! # How
//!
//! [`SubBars`](super::fill_sim::SubBars) is deliberately **sync** so no async is
//! threaded through the sim, and that stays true:
//!
//! 1. **Pass 1** runs the sim with [`RecordingSubBars`], which returns no
//!    candles (behaving exactly like [`NoZoom`](super::fill_sim::NoZoom)) but
//!    *records every window it was asked for*.
//! 2. The caller fetches the finer series for those recorded windows only.
//! 3. **Pass 2** re-runs the sim with a [`WindowSubBars`] built from that fetch.
//!
//! The recorder deliberately derives the window set from the sim ITSELF rather
//! than re-deriving "which bars look ambiguous" alongside it. A second
//! hand-written straddle test would be a copy of the `(true, true)` arm in
//! `simulate_fill_resolved_zoom` that has to agree with it by hand — exactly the
//! kind of parallel derivation that drifts. Here pass 1 asks the real question.
//!
//! # The one thing that makes this sound
//!
//! Pass 1 must ask for the SAME window pass 2 needs. That holds because the
//! straddle test (`hit_sl && hit_tp`) is computed from the coarse bar, the
//! resolved bracket, and the widen/break-even state — none of which depend on
//! the sub-bars. A provider can only change what happens *after* the window is
//! requested. `zoom_requests_are_invariant_to_the_provider` pins that down.
//!
//! # Fixtures do NOT store the finer bars — and must not start, naively
//!
//! `fixture::save` freezes the plan, the **coarse** candle window, meta, and the
//! expected outcome. The finer series has never been persisted: a frozen fixture
//! replays with NO zoom provider at all (`replay::run(.., None)`), so an
//! ambiguous bar keeps the pessimistic stop, which is how its `expected.json`
//! was computed. The eager pull used to fetch ~22k M1 bars during a `--save` run
//! and then discard every one of them.
//!
//! If sub-bar support is ever added to fixtures, do NOT freeze whatever this
//! module happened to fetch. The recorded windows are a function of the CURRENT
//! strategy, bracket and widen state — change any of them and different bars go
//! ambiguous, so a saved narrow series would be missing exactly the bars the new
//! run asks for, and the zoom would silently degrade to the pessimistic stop on
//! a fixture that looks complete. A fixture that wants offline zoom needs the
//! finer bars for the whole window (an explicit, deliberately larger artifact),
//! not the lazy subset.

use chrono::{DateTime, Utc};
use std::cell::RefCell;

use super::fill_sim::SubBars;
use trade_control_core::broker::BidAskCandle;

/// One coarse parent bar's sub-window, `[start, end)` — what the sim asks for
/// when it hits an ambiguous SL/TP bar, and therefore the only span the finer
/// feed needs to cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ZoomWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// A [`SubBars`] provider that serves NOTHING but records what it was asked for.
///
/// Behaviourally identical to [`NoZoom`](super::fill_sim::NoZoom) — pass 1 keeps
/// the pessimistic stop — so its only effect is the recording. Interior
/// mutability because `SubBars::sub_bars` takes `&self` (the sim holds it as a
/// `&dyn SubBars`).
#[derive(Debug, Default)]
pub struct RecordingSubBars {
    windows: RefCell<Vec<ZoomWindow>>,
}

impl RecordingSubBars {
    pub fn new() -> Self {
        Self::default()
    }

    /// The windows requested so far, de-duplicated and in ascending order.
    ///
    /// Sorted + deduped because this drives a fetch: the caller wants a stable,
    /// minimal set, not a call log. In practice the sim returns on the first
    /// ambiguous bar so this is 0 or 1 windows per simulated entry, but nothing
    /// here depends on that.
    pub fn windows(&self) -> Vec<ZoomWindow> {
        let mut out = self.windows.borrow().clone();
        out.sort();
        out.dedup();
        out
    }
}

impl SubBars for RecordingSubBars {
    fn sub_bars(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<BidAskCandle> {
        self.windows.borrow_mut().push(ZoomWindow { start, end });
        // Serve nothing: pass 1 must behave exactly like `NoZoom`, so the bars
        // it reports are the bars the sim genuinely could not resolve on its own.
        Vec::new()
    }
}

/// Delegation so the driver can hand the replay an `Rc<RecordingSubBars>` as its
/// `SubBars` provider while keeping its own handle to read the windows back
/// afterwards. (The replay takes ownership of the boxed provider, so a borrow
/// won't do.)
impl<T: SubBars + ?Sized> SubBars for std::rc::Rc<T> {
    fn sub_bars(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<BidAskCandle> {
        (**self).sub_bars(start, end)
    }
}

/// A [`SubBars`] provider backed by an already-fetched finer series.
///
/// This is what pass 2 uses. It holds whatever the narrow fetch returned and
/// filters by window on each request — the same contract the eager whole-window
/// pull satisfied, just over far fewer candles.
#[derive(Debug, Default)]
pub struct WindowSubBars {
    /// Ascending by time. Filtering a small slice is cheaper than the index that
    /// would replace it, since a zoom asks for 0–2 windows per replay.
    candles: Vec<BidAskCandle>,
}

impl WindowSubBars {
    /// Build from a fetched finer series. Sorted defensively: the contract of
    /// `sub_bars` is "ascending", and a caller that concatenates several fetched
    /// windows would otherwise hand over a series ordered by fetch, not by time
    /// — which would silently mis-order the zoom's first-touch decision.
    pub fn new(mut candles: Vec<BidAskCandle>) -> Self {
        candles.sort_by_key(|c| c.time);
        Self { candles }
    }
}

impl SubBars for WindowSubBars {
    fn sub_bars(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<BidAskCandle> {
        self.candles
            .iter()
            .filter(|c| c.time >= start && c.time < end)
            .cloned()
            .collect()
    }
}

/// Merge windows that touch or overlap, so two ambiguous bars that happen to be
/// adjacent cost ONE broker request rather than two.
///
/// Input need not be sorted; output is ascending and disjoint.
pub fn coalesce(mut windows: Vec<ZoomWindow>) -> Vec<ZoomWindow> {
    windows.sort();
    let mut out: Vec<ZoomWindow> = Vec::new();
    for w in windows {
        match out.last_mut() {
            // `>=` not `>`: half-open windows that merely touch (`a.end ==
            // b.start`) are contiguous in time, so merging them is still one
            // continuous span — and one fetch instead of two.
            Some(prev) if w.start <= prev.end => prev.end = prev.end.max(w.end),
            _ => out.push(w),
        }
    }
    out
}

/// Fetch the finer series for `windows` only — the narrow half of the lazy zoom.
///
/// Fail-soft per window, matching the eager pull's contract: a window that errors
/// contributes nothing and is logged, rather than failing the replay. A caller
/// that gets an empty result keeps the pessimistic stop, exactly as
/// [`NoZoom`](super::fill_sim::NoZoom) does — the zoom only ever REDUCES
/// ambiguity, so missing finer data can never make a replay wrong, only more
/// conservative.
pub async fn fetch_windows(
    source: trade_control_cli::replay_args::CandleSource,
    symbol: &str,
    finer: super::granularity::ReplayGranularity,
    windows: &[ZoomWindow],
    cache_dir: Option<std::path::PathBuf>,
) -> Vec<BidAskCandle> {
    let mut out = Vec::new();
    for w in coalesce(windows.to_vec()) {
        match super::candles::pull(source, symbol, finer, w.start, w.end, cache_dir.clone()).await {
            Ok(cs) => out.extend(cs),
            Err(e) => tracing::warn!(
                error = %e,
                start = %w.start,
                end = %w.end,
                "sub-bar zoom: finer pull failed for this window — that bar keeps \
                 the pessimistic stop"
            ),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid RFC3339")
    }

    fn w(start: &str, end: &str) -> ZoomWindow {
        ZoomWindow {
            start: ts(start),
            end: ts(end),
        }
    }

    #[test]
    fn coalesce_merges_touching_and_overlapping_windows() {
        // 13:00-14:00 and 14:00-15:00 merely TOUCH — half-open, so they're one
        // continuous span and must become a single fetch.
        // 16:00-17:00 overlaps 16:30-18:00.
        // 20:00-21:00 stands alone.
        let got = coalesce(vec![
            w("2026-06-17T16:00:00Z", "2026-06-17T17:00:00Z"),
            w("2026-06-17T13:00:00Z", "2026-06-17T14:00:00Z"),
            w("2026-06-17T20:00:00Z", "2026-06-17T21:00:00Z"),
            w("2026-06-17T14:00:00Z", "2026-06-17T15:00:00Z"),
            w("2026-06-17T16:30:00Z", "2026-06-17T18:00:00Z"),
        ]);
        assert_eq!(
            got,
            vec![
                w("2026-06-17T13:00:00Z", "2026-06-17T15:00:00Z"),
                w("2026-06-17T16:00:00Z", "2026-06-17T18:00:00Z"),
                w("2026-06-17T20:00:00Z", "2026-06-17T21:00:00Z"),
            ]
        );
    }

    #[test]
    fn coalesce_keeps_a_gap_between_disjoint_windows() {
        // A real session gap between two ambiguous bars must NOT be bridged —
        // bridging would re-introduce exactly the wide fetch this module exists
        // to avoid (here: a 6-hour span instead of two 1-hour ones).
        let got = coalesce(vec![
            w("2026-06-17T13:00:00Z", "2026-06-17T14:00:00Z"),
            w("2026-06-17T19:00:00Z", "2026-06-17T20:00:00Z"),
        ]);
        assert_eq!(got.len(), 2, "disjoint windows must stay separate: {got:?}");
    }

    #[test]
    fn coalesce_of_nothing_is_nothing() {
        // The common case — no ambiguous bar ⇒ no windows ⇒ no fetch at all.
        assert!(coalesce(Vec::new()).is_empty());
    }

    #[test]
    fn recorder_serves_nothing_so_pass_one_matches_nozoom() {
        let rec = RecordingSubBars::new();
        let out = rec.sub_bars(ts("2026-06-17T13:00:00Z"), ts("2026-06-17T14:00:00Z"));
        assert!(
            out.is_empty(),
            "pass 1 must serve no candles, else it is not NoZoom-equivalent"
        );
    }

    #[test]
    fn recorder_dedups_and_sorts_repeated_requests() {
        let rec = RecordingSubBars::new();
        // Ask out of order, and twice for the same window.
        rec.sub_bars(ts("2026-06-17T19:00:00Z"), ts("2026-06-17T20:00:00Z"));
        rec.sub_bars(ts("2026-06-17T13:00:00Z"), ts("2026-06-17T14:00:00Z"));
        rec.sub_bars(ts("2026-06-17T19:00:00Z"), ts("2026-06-17T20:00:00Z"));
        assert_eq!(
            rec.windows(),
            vec![
                w("2026-06-17T13:00:00Z", "2026-06-17T14:00:00Z"),
                w("2026-06-17T19:00:00Z", "2026-06-17T20:00:00Z"),
            ]
        );
    }
}
