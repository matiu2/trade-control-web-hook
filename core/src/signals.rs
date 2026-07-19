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
mod detect;
mod metrics;
mod state_machine;

pub use atr::{atr_length_for, wilder_atr};
pub use detect::{DetectFlags, Detected, SignalGeometry, detect_at};
pub use metrics::CandleMetrics;
pub use state_machine::{
    DetectorConfig, LatchedSignal, first_confirmed_signal_at, latched_signal_at,
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
