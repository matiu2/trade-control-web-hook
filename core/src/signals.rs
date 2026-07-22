//! Server-side port of the TradingView `candle-signals-v2.pine` detector.
//!
//! # Why this exists
//!
//! The H&S `05-enter` (and `06-close-on-…`) trades fire on the Pine
//! "Long Pattern" / "Short Pattern" alertconditions: a candle-pattern detector
//! (pinbar / tweezer / double-tweezer / regular-engulfer / floating-engulfer)
//! plus a small per-signal state machine (pending → valid → invalid, with a
//! confirmation latch and a golden-protected opposing-signal invalidation rule).
//! When a signal validates, Pine substitutes the latched **signal geometry**
//! (`signal_high`/`signal_low`/`signal_range`/`signal_kind`/`golden`/
//! `signal_confirmed`/`recent_high`/`recent_low`/`atr`) into the alert message.
//! The worker then resolves the enter's entry/SL/TP against those fields (see
//! [`crate::intent::PriceAnchor::SignalHigh`] etc.).
//!
//! To evaluate the H&S entry **server-side** (Stage E of the engine plan), this
//! module reproduces that detector in Rust. It is **pure** — a function of a
//! candle slice — and deliberately **stateless across cron ticks**: each tick
//! recomputes the latched signal from a back-window of recent closed candles
//! (decision in the Stage-E plan). No new KV.
//!
//! # Layout
//!
//! - [`metrics`] — per-candle derived quantities (range, body, wicks, the 25%
//!   bands, close-position) mirroring the Pine "Common Calculations" block.
//! - [`atr`] — Wilder ATR with the timeframe-dependent length from
//!   `f_get_atr_length()`.
//! - [`detect`] — the five single-bar/2-bar/3-bar pattern detectors and the
//!   per-bar [`SignalGeometry`] (extremes, range, kind, start time) that prints
//!   when a bar satisfies one.
//! - [`state_machine`] — the recompute-from-window driver that runs the
//!   pending/valid/invalid state machine and returns the **latched** signal as
//!   of a given as-of bar, the value the alert would have carried.
//!
//! # Intentional divergence from current Pine (bug #10B)
//!
//! Pine's confirmation can latch `signal_confirmed = 1` off a not-yet-closed
//! bar (the ADIDAS 5:30-vs-5:45 case — see the `hs_enter_anchors_signal_levels`
//! analysis, finding B). The engine only ever sees **closed** candles, so the
//! port confirms only on a fully-closed pushing bar within `confirm_bars`. This
//! is a deliberate, correct divergence; the historical-replay follow-up will
//! show the diff on that case.

use crate::broker::Granularity;

mod atr;
mod band_anchor;
mod detect;
mod metrics;
mod state_machine;

pub use atr::{atr_length_for, wilder_atr};
pub use band_anchor::band_anchor;
pub use detect::{DetectFlags, Detected, SignalGeometry, detect_at};
pub use metrics::CandleMetrics;
pub use state_machine::{
    DetectorConfig, LatchedSignal, SignalCriteria, first_confirmed_signal_at, latched_signal_at,
};

// `detector_lookback_bars` is defined below (shared by live + replay).

/// The default detector config the H&S chart study ships with (`confirm_bars =
/// 2`, `sl_lookback = 5`, `similarity_pct = 20`, all five patterns on). Matches
/// the `input.*` defaults in `candle-signals-v2.pine`.
pub fn default_config(granularity: Granularity) -> DetectorConfig {
    DetectorConfig::pine_defaults(granularity)
}

/// Bars of history the detector needs behind a candidate signal bar to resolve
/// its confirmation, pattern depth, and SL-lookback window. The engine sizes
/// its back-window fetch by this so a freshly-armed plan can detect a signal
/// near the window's leading edge.
pub fn min_lookback_bars(cfg: &DetectorConfig) -> usize {
    // 3 bars of pattern depth (double-tweezer) + the SL lookback ahead of the
    // signal + the confirm window after it, with a little slack.
    3 + cfg.sl_lookback + cfg.confirm_bars + 2
}

/// Bars of history the detector needs behind a candidate signal bar to produce a
/// **correct golden verdict** — the single source of truth for the detector
/// back-window depth, called by BOTH the live worker (`pine_lookback_since`) and
/// the offline replay warmup floor so the two can never drift by caller.
///
/// The golden flag is `body_size >= ATR` ([`detect::Detected::is_golden`]), and
/// [`wilder_atr`] returns `None` — silently forcing `golden = false` — when the
/// window is shorter than [`atr_length_for`] (24 bars on H1, 96 on M15). So a
/// window sized only by [`min_lookback_bars`] (~12) is enough to *detect the
/// pattern* but too short to *warm the ATR*, and every `needs_golden` enter is
/// wrongly declined "needs golden but signal is not golden". Taking the max of
/// the two requirements fixes that: the pattern state machine and the ATR are
/// both satisfied. The `+2` slack mirrors `min_lookback_bars`' own slack (the
/// leading edge of the fetched window is the least reliable bar).
pub fn detector_lookback_bars(cfg: &DetectorConfig, granularity: Granularity) -> usize {
    min_lookback_bars(cfg).max(atr_length_for(granularity) + 2)
}

/// The inclusive lower bound for the "first confirmed signal" scan
/// ([`first_confirmed_signal_at`]'s `not_before`), given the detector window and
/// the as-of bar index — the single source of truth shared by the live worker
/// and the offline replay so the scan can never drift by caller (bug ①).
///
/// `explicit` is the setup floor the engine already has (the later of the
/// break-and-close time and the `tv-arm --start` replay cursor); when present it
/// always wins. When it is `None`, the scan must NOT fall through to "consider
/// the whole window" — the window depth **legitimately varies by caller** (a
/// trendline-anchored plan fetches back to an old neckline on *both* live and
/// replay; a `--warmup-bars` replay is always deep). An unbounded `None` scan
/// then latches an ancient warmup-era confirmed signal in a deep window but a
/// recent one in a shallow window — the exact replay≠live split. So the fallback
/// floor is derived from the SHARED [`detector_lookback_bars`] depth behind the
/// as-of bar: the recent setup window, independent of how far back the *fetch*
/// reached. `saturating_sub` floors to bar 0 for an early as-of (short window ⇒
/// whole window in scope, no panic).
pub fn confirmed_scan_floor(
    window: &[chrono::DateTime<chrono::Utc>],
    as_of: usize,
    cfg: &DetectorConfig,
    granularity: Granularity,
    explicit: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    if explicit.is_some() {
        return explicit;
    }
    if window.is_empty() {
        return None;
    }
    let as_of = as_of.min(window.len() - 1);
    let back = detector_lookback_bars(cfg, granularity);
    Some(window[as_of.saturating_sub(back)])
}

#[cfg(test)]
mod lookback_tests {
    use super::*;

    /// On H1 the ATR length (24) dominates the ~12-bar pattern lookback, so the
    /// detector window must reach back at least `atr_length + slack` — otherwise
    /// `wilder_atr` returns `None` and every golden is forced false (the live
    /// ATR-starvation bug).
    #[test]
    fn h1_window_reaches_the_atr_length() {
        let cfg = DetectorConfig::pine_defaults(Granularity::H1);
        let bars = detector_lookback_bars(&cfg, Granularity::H1);
        assert!(
            bars >= atr_length_for(Granularity::H1),
            "H1 detector window {bars} must cover the ATR length {}",
            atr_length_for(Granularity::H1)
        );
        // And it must dominate the pattern-only lookback that caused the bug.
        assert!(bars > min_lookback_bars(&cfg));
    }

    /// M15's ATR length (96) is far larger than the pattern lookback, so the
    /// gap the bug exploited is widest here.
    #[test]
    fn m15_window_reaches_the_atr_length() {
        let cfg = DetectorConfig::pine_defaults(Granularity::M15);
        let bars = detector_lookback_bars(&cfg, Granularity::M15);
        assert!(bars >= atr_length_for(Granularity::M15));
        assert!(bars > min_lookback_bars(&cfg));
    }

    /// bug ①: with no explicit floor, a DEEP window must scope the scan to the
    /// recent `detector_lookback_bars`, NOT to the whole (ancient) tail — so live
    /// and replay pick the same signal regardless of fetch depth.
    #[test]
    fn confirmed_scan_floor_scopes_a_deep_window_to_the_recent_lookback() {
        use chrono::{TimeZone, Utc};
        let g = Granularity::H1;
        let cfg = DetectorConfig::pine_defaults(g);
        // 200-bar deep window (a --warmup-bars replay), hourly.
        let window: Vec<_> = (0..200)
            .map(|i| Utc.timestamp_opt(i * 3600, 0).unwrap())
            .collect();
        let as_of = 199;
        let floor = confirmed_scan_floor(&window, as_of, &cfg, g, None)
            .expect("a non-empty window yields a floor");
        // The floor is exactly detector_lookback_bars behind the as-of bar — not
        // bar 0. An ancient signal earlier than that cannot claim the winner slot.
        let back = detector_lookback_bars(&cfg, g);
        assert_eq!(floor, window[as_of - back]);
        assert!(floor > window[0], "must not be the ancient window start");
    }

    /// An explicit floor (break-and-close / replay_start) always wins over the
    /// derived fallback.
    #[test]
    fn confirmed_scan_floor_prefers_an_explicit_floor() {
        use chrono::{TimeZone, Utc};
        let g = Granularity::H1;
        let cfg = DetectorConfig::pine_defaults(g);
        let window: Vec<_> = (0..50)
            .map(|i| Utc.timestamp_opt(i * 3600, 0).unwrap())
            .collect();
        let explicit = Utc.timestamp_opt(40 * 3600, 0).unwrap();
        assert_eq!(
            confirmed_scan_floor(&window, 49, &cfg, g, Some(explicit)),
            Some(explicit)
        );
    }

    /// A window shallower than `detector_lookback_bars` (an early as-of) floors to
    /// bar 0 — the whole (short) window is in scope, no panic.
    #[test]
    fn confirmed_scan_floor_saturates_to_bar_zero_on_a_short_window() {
        use chrono::{TimeZone, Utc};
        let g = Granularity::H1;
        let cfg = DetectorConfig::pine_defaults(g);
        let window: Vec<_> = (0..5)
            .map(|i| Utc.timestamp_opt(i * 3600, 0).unwrap())
            .collect();
        // as_of 4, lookback ~26 > window len → saturating_sub floors to bar 0.
        assert_eq!(
            confirmed_scan_floor(&window, 4, &cfg, g, None),
            Some(window[0])
        );
    }

    /// An empty window yields no floor (defensive; the engine won't scan it).
    #[test]
    fn confirmed_scan_floor_empty_window_is_none() {
        let g = Granularity::H1;
        let cfg = DetectorConfig::pine_defaults(g);
        assert_eq!(confirmed_scan_floor(&[], 0, &cfg, g, None), None);
    }

    /// The shared depth is never *shorter* than the pattern lookback on any
    /// granularity — it only ever widens the window, never narrows it.
    #[test]
    fn never_shorter_than_min_lookback() {
        for g in [
            Granularity::M1,
            Granularity::M5,
            Granularity::M15,
            Granularity::H1,
            Granularity::H4,
            Granularity::D1,
        ] {
            let cfg = DetectorConfig::pine_defaults(g);
            assert!(detector_lookback_bars(&cfg, g) >= min_lookback_bars(&cfg));
        }
    }
}
