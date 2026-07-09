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
