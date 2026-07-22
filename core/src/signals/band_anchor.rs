//! The **band anchor** — the single price a reversal-close's S/R-band test keys
//! on, chosen per pattern so it represents where the candle *rejected off* the
//! level (bounced back out of the zone), not merely where it closed.
//!
//! # Why this exists
//!
//! `07-close-on-sr-reversal` closes an open position when a golden opposing
//! reversal candle prints **off a drawn S/R level** — the candle's price must
//! sit inside one of the intent's `sr_bands`. The band check used to test the
//! candle's *close*, which fired on a bar that merely *fell into* the zone
//! (continuation) rather than *bounced out of* it (the intended reversal). The
//! UK 100 long on 2026-07-17T01:00:00Z was a bearish engulfer that opened
//! ~16 pts above the band and closed inside it — a continuation bar the
//! close-in-band test wrongly flagged as an off-the-level reversal.
//!
//! The fix keys the band test on the part of the candle that is the *rejection
//! point* for that pattern:
//!
//! | pattern | anchor | intuition |
//! |---|---|---|
//! | `RegularEngulfer` / `FloatingEngulfer` | **open** | opened at/into the level, engulfed back out |
//! | `Pinbar` / `Tweezer` / `DoubleTweezer`  | **wick 50%** | the wick is the rejection; its midpoint must merge with the band |
//!
//! Wick-50% is direction-aware — a reversal-close of a **long** fires on a
//! **short** (bearish, upper-wick) signal, and vice-versa:
//!
//! - **Short** (bearish, upper-wick rejection): `body_top + (high - body_top) / 2`
//! - **Long**  (bullish, lower-wick rejection): `body_bot - (body_bot - low) / 2`
//!
//! where `body_top = max(open, close)` and `body_bot = min(open, close)`.
//!
//! # Replay == live
//!
//! Both consumers compute the anchor from the **same** inputs — the reversal
//! candle's OHLC plus its `SignalKind`/direction — so the engine (replay) and the
//! live worker can't drift (`[[strategy_changes_in_both_replayer_and_worker]]`):
//!
//! - **Engine** (`engine/src/evaluate.rs::close_windows_pass`) calls
//!   [`band_anchor`] directly with the candle OHLC and the latched signal's
//!   `kind`/`direction`.
//! - **Worker** (`core/src/dispatch/close.rs::run_close`) calls
//!   [`crate::intent::Shell::band_anchor`], which reads `signal_kind` + OHLC off
//!   the shell the cron engine built via `Shell::from_candle_and_signal`.
//!
//! No new signed wire field is needed: the shell already carries `signal_kind`
//! and `open`, and the engine already holds the `LatchedSignal`.

use crate::intent::{Direction, SignalKind};

/// The price a reversal-close's S/R-band test should use for a candle with the
/// given pattern `kind`, reversal `dir`ection, and OHLC.
///
/// See the module docs for the per-pattern rule. `dir` is the **signal**
/// direction (the direction of the reversal candle) — for a long-position
/// close that is `Short`, for a short-position close it is `Long`.
pub fn band_anchor(
    kind: SignalKind,
    dir: Direction,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
) -> f64 {
    match kind {
        // Engulfers: the open is where price sat at/into the level before
        // engulfing back out of the zone.
        SignalKind::RegularEngulfer | SignalKind::FloatingEngulfer => open,
        // Wick-rejection patterns: the midpoint of the rejection wick must merge
        // with the band. The wick runs from the body edge to the extreme.
        SignalKind::Pinbar | SignalKind::Tweezer | SignalKind::DoubleTweezer => {
            let body_top = open.max(close);
            let body_bot = open.min(close);
            match dir {
                // Bearish reversal: long upper wick from body_top up to the high.
                Direction::Short => body_top + (high - body_top) / 2.0,
                // Bullish reversal: long lower wick from body_bot down to the low.
                Direction::Long => body_bot - (body_bot - low) / 2.0,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engulfer_anchors_on_open() {
        // The UK 100 case: bearish engulfer opened above the band, closed in it.
        // Anchor is the open (10551.7), which is out of band — no fire.
        let a = band_anchor(
            SignalKind::RegularEngulfer,
            Direction::Short,
            10551.7,
            10559.7,
            10532.1,
            10532.9,
        );
        assert!((a - 10551.7).abs() < 1e-9);
        let a = band_anchor(
            SignalKind::FloatingEngulfer,
            Direction::Short,
            10551.7,
            10559.7,
            10532.1,
            10532.9,
        );
        assert!((a - 10551.7).abs() < 1e-9);
    }

    #[test]
    fn short_pinbar_anchors_on_upper_wick_midpoint() {
        // Bearish pinbar: body at the bottom, long upper wick.
        // open=10, close=11 (small body), high=20, low=9.5.
        // body_top=11, midpoint = 11 + (20-11)/2 = 15.5.
        let a = band_anchor(SignalKind::Pinbar, Direction::Short, 10.0, 20.0, 9.5, 11.0);
        assert!((a - 15.5).abs() < 1e-9);
    }

    #[test]
    fn long_pinbar_anchors_on_lower_wick_midpoint() {
        // Bullish pinbar: body at the top, long lower wick.
        // open=19, close=18 (small body), high=20.5, low=10.
        // body_bot=18, midpoint = 18 - (18-10)/2 = 14.
        let a = band_anchor(SignalKind::Pinbar, Direction::Long, 19.0, 20.5, 10.0, 18.0);
        assert!((a - 14.0).abs() < 1e-9);
    }

    #[test]
    fn tweezer_and_double_follow_pinbar_rule() {
        // Same geometry as short_pinbar — tweezer/double are wick-rejection kinds.
        for kind in [SignalKind::Tweezer, SignalKind::DoubleTweezer] {
            let a = band_anchor(kind, Direction::Short, 10.0, 20.0, 9.5, 11.0);
            assert!((a - 15.5).abs() < 1e-9, "kind {kind:?}");
        }
    }
}
