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
//! # Fixtures store the sub-bars the zoom actually looked at
//!
//! `fixture::save` freezes the plan, the coarse candle window, meta, the expected
//! outcome — and, when the run had an ambiguous bar, `sub_bars.json`: exactly the
//! finer candles this module fetched. So a fixture whose verdict depended on a
//! zoom reproduces that verdict offline instead of degrading to the pessimistic
//! stop. A fixture with no ambiguous bar (the common case) writes no such file
//! and is byte-identical to one saved before sub-bars existed.
//!
//! Deliberately the *lazy subset*, not the whole finer window: M1 across every
//! fixture's span would be ~416 MB for this corpus versus ~1 MB for the bars the
//! zoom can actually consume.
//!
//! **The stored set is a function of the strategy that saved it.** Change the
//! bracket, widen, or entry rule and a *different* bar can go ambiguous — one the
//! fixture has no bars for. [`FixtureSubBars`] detects that (it reports windows
//! outside the saved extent as misses) and the fixture path refetches them, so a
//! stale fixture recovers instead of silently scoring those bars as stops.

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

/// A [`SubBars`] provider over a fixture's SAVED finer candles, which also
/// records any window it could not cover.
///
/// A frozen fixture stores only the windows the zoom asked for on the run that
/// saved it. Change the strategy (bracket, widen, entry rule) and a *different*
/// bar can go ambiguous — one the fixture has no bars for. Serving an empty slice
/// there would silently degrade that bar to the pessimistic stop, on a fixture
/// that looks complete.
///
/// So this distinguishes the two cases that both "look empty":
///
/// - the window is **covered** (it lies inside a stored window's span) and simply
///   has no ticks ⇒ a legitimate empty answer, no miss recorded;
/// - the window is **not covered** at all ⇒ recorded as a miss, so the caller can
///   fetch it and re-run.
///
/// Coverage is judged against the stored windows' spans rather than "did any
/// candle come back", because an illiquid cross legitimately has minutes with no
/// candle at all — the exact case that must NOT be mistaken for missing data.
#[derive(Debug)]
pub struct FixtureSubBars {
    inner: WindowSubBars,
    /// Spans the saved candles are known to cover, ascending and disjoint.
    covered: Vec<ZoomWindow>,
    missed: RefCell<Vec<ZoomWindow>>,
}

impl FixtureSubBars {
    /// Build from a fixture's saved finer candles.
    ///
    /// Coverage spans the saved candles' **extent** — first stored candle to last
    /// (plus one finer bar) — NOT each candle individually.
    ///
    /// The distinction matters and is the whole point of this type. A saved
    /// window legitimately contains minutes with no candle: an illiquid cross
    /// simply doesn't tick every minute (CAD/SGD lost 200 of 1440 on a normal
    /// day). Deriving coverage per-candle would split the span at every such
    /// minute and report it as missing — the exact false-miss that would refetch
    /// those minutes on every run, which is the candle-cache pathology this whole
    /// change exists to stop paying.
    ///
    /// Using the extent means a window inside the saved range is "covered" even
    /// when empty, while a window outside it is a genuine miss. That's the only
    /// question the caller needs answered.
    pub fn new(candles: Vec<BidAskCandle>, bar_len: chrono::Duration) -> Self {
        let inner = WindowSubBars::new(candles);
        let covered = match (inner.candles.first(), inner.candles.last()) {
            (Some(first), Some(last)) => vec![ZoomWindow {
                start: first.time,
                end: last.time + bar_len,
            }],
            // Nothing stored ⇒ nothing covered ⇒ every window asked for is a miss.
            _ => Vec::new(),
        };
        Self {
            inner,
            covered,
            missed: RefCell::new(Vec::new()),
        }
    }

    /// Windows the sim asked for that the saved candles don't cover, deduped and
    /// ascending. Empty ⇒ the fixture had everything the zoom needed.
    pub fn missed(&self) -> Vec<ZoomWindow> {
        let mut out = self.missed.borrow().clone();
        out.sort();
        out.dedup();
        out
    }
}

impl SubBars for FixtureSubBars {
    fn sub_bars(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<BidAskCandle> {
        let covered = self
            .covered
            .iter()
            .any(|w| start >= w.start && end <= w.end);
        if !covered {
            self.missed.borrow_mut().push(ZoomWindow { start, end });
        }
        self.inner.sub_bars(start, end)
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

    // --- FixtureSubBars: saved bars, and knowing when they're NOT enough -----

    fn m1(time: &str) -> BidAskCandle {
        let t = ts(time);
        BidAskCandle {
            time: t,
            o: 1.0,
            h: 1.0,
            l: 1.0,
            c: 1.0,
            bid_o: 1.0,
            bid_h: 1.0,
            bid_l: 1.0,
            bid_c: 1.0,
            ask_o: 1.0,
            ask_h: 1.0,
            ask_l: 1.0,
            ask_c: 1.0,
        }
    }

    const MIN: chrono::Duration = chrono::Duration::minutes(1);

    #[test]
    fn fixture_sub_bars_serves_a_covered_window_and_records_no_miss() {
        // Saved M1 bars spanning 13:00..13:03.
        let f = FixtureSubBars::new(
            vec![
                m1("2026-06-17T13:00:00Z"),
                m1("2026-06-17T13:01:00Z"),
                m1("2026-06-17T13:02:00Z"),
            ],
            MIN,
        );
        let got = f.sub_bars(ts("2026-06-17T13:00:00Z"), ts("2026-06-17T13:03:00Z"));
        assert_eq!(got.len(), 3);
        assert!(f.missed().is_empty(), "missed: {:?}", f.missed());
    }

    #[test]
    fn fixture_sub_bars_records_an_uncovered_window_as_a_miss() {
        // The fixture saved 13:00..13:03, but the strategy changed and a bar at
        // 19:00 is now ambiguous. That must be reported, not silently served as
        // "no finer data" (which would degrade it to the pessimistic stop on a
        // fixture that looks complete).
        let f = FixtureSubBars::new(vec![m1("2026-06-17T13:00:00Z")], MIN);
        let got = f.sub_bars(ts("2026-06-17T19:00:00Z"), ts("2026-06-17T20:00:00Z"));
        assert!(got.is_empty());
        assert_eq!(
            f.missed(),
            vec![w("2026-06-17T19:00:00Z", "2026-06-17T20:00:00Z")]
        );
    }

    /// The distinction the whole design turns on: a covered window that happens
    /// to contain NO candles is a legitimate answer (an illiquid cross has
    /// minutes that never ticked), NOT missing data. Judging coverage by "did any
    /// candle come back" would re-fetch those forever — the exact candle-cache
    /// pathology this work exists to stop paying.
    #[test]
    fn a_covered_but_empty_window_is_not_a_miss() {
        // A CONTIGUOUS run 13:00..13:04 (four M1 bars) — one covered span — with
        // one minute deliberately absent inside it: 13:02 never ticked, exactly
        // as an illiquid cross behaves (CAD/SGD lost 200 of 1440 minutes on a
        // normal trading day). The saved fixture is complete; that minute simply
        // has no candle to store.
        let f = FixtureSubBars::new(
            vec![
                m1("2026-06-17T13:00:00Z"),
                m1("2026-06-17T13:01:00Z"),
                // 13:02 — no tick, no candle
                m1("2026-06-17T13:03:00Z"),
            ],
            MIN,
        );

        // Asking about the absent minute returns nothing — and that must NOT be
        // recorded as a miss. It lies inside the covered span, so the answer
        // "there is no candle here" is the truth, not missing data. Judging
        // coverage by "did candles come back" gets this wrong and would refetch
        // that minute on every single run, forever.
        let got = f.sub_bars(ts("2026-06-17T13:02:00Z"), ts("2026-06-17T13:03:00Z"));
        assert!(got.is_empty(), "13:02 has no candle by construction");
        assert!(
            f.missed().is_empty(),
            "a covered-but-empty window must not be a miss — got {:?}",
            f.missed()
        );

        // A window PAST the covered span is a genuine miss.
        f.sub_bars(ts("2026-06-17T19:00:00Z"), ts("2026-06-17T19:01:00Z"));
        assert_eq!(
            f.missed(),
            vec![w("2026-06-17T19:00:00Z", "2026-06-17T19:01:00Z")]
        );
    }

    /// Known, deliberate limitation of using the saved candles' EXTENT as the
    /// covered span: a fixture that saved two widely-separated windows counts
    /// the gap between them as covered, so a newly-ambiguous bar landing in that
    /// gap is served empty (pessimistic stop) instead of being refetched.
    ///
    /// Accepted because the alternative is strictly worse. Distinguishing "gap
    /// between two fetched windows" from "minute that never ticked" is not
    /// possible from the candles alone, and treating absent minutes as missing
    /// would refetch them on every run — the pathology this change exists to fix.
    /// The failure here is conservative (an over-pessimistic bar on a stale
    /// fixture); the other way round is an unbounded refetch loop.
    ///
    /// If this ever bites, the fix is to store the fetched WINDOWS alongside the
    /// candles rather than inferring them.
    #[test]
    fn a_gap_between_two_saved_windows_reads_as_covered_known_limitation() {
        let f = FixtureSubBars::new(
            vec![m1("2026-06-17T13:00:00Z"), m1("2026-06-17T19:00:00Z")],
            MIN,
        );
        f.sub_bars(ts("2026-06-17T16:00:00Z"), ts("2026-06-17T16:01:00Z"));
        assert!(
            f.missed().is_empty(),
            "documenting current behaviour: the extent spans the gap, so this is \
             NOT reported as a miss"
        );
    }

    #[test]
    fn a_fixture_with_no_saved_sub_bars_misses_every_window_it_is_asked_for() {
        // The pre-sub-bar case: nothing stored. Any ambiguous bar is a miss, so
        // the caller re-fetches rather than assuming the pessimistic stop.
        let f = FixtureSubBars::new(Vec::new(), MIN);
        f.sub_bars(ts("2026-06-17T13:00:00Z"), ts("2026-06-17T14:00:00Z"));
        assert_eq!(f.missed().len(), 1);
    }
}
