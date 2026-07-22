//! The **band anchor** — the single price a reversal-close's S/R-band test keys
//! on, chosen per pattern so it represents where the candle *rejected off* the
//! level (bounced back out of the zone), not merely where it closed.
//!
//! # Why this exists
//!
//! `07-close-on-sr-reversal` closes an open position when a golden opposing
//! reversal candle prints **off a drawn S/R level** — the anchor price must sit
//! inside one of the intent's `sr_bands`. The band check used to test the
//! candle's *close*, which fired on a bar that merely *fell into* the zone
//! (continuation) rather than *bounced out of* it (the intended reversal). The
//! UK 100 long on 2026-07-17T01:00:00Z was a bearish engulfer that opened
//! ~16 pts above the band and closed inside it — a continuation bar the
//! close-in-band test wrongly flagged as an off-the-level reversal.
//!
//! The fix keys the band test on the part of the pattern that is its *rejection
//! point*:
//!
//! | pattern | anchor | intuition |
//! |---|---|---|
//! | `RegularEngulfer` / `FloatingEngulfer` | **open of the pattern's FIRST bar** | the pair originated at the level, then engulfed back out |
//! | `Pinbar` / `Tweezer` / `DoubleTweezer`  | **wick 50%** of the print bar | the wick is the rejection; its midpoint must merge with the band |
//!
//! **Which bar's open for an engulfer.** A regular/floating engulfer spans two
//! bars — the *first* bar (`N-1`) is where price sat at/into the level, the
//! second (`N`, the print bar) is the one that closed beyond the first's
//! extreme. The origin at the level is the **first bar's open**, so that is the
//! engulfer anchor (operator, 2026-07-22). A one-bar pinbar has no earlier bar,
//! so its "origin" *is* the print bar; tweezers are wick patterns and key off
//! the print bar's wick regardless.
//!
//! Wick-50% is direction-aware — a reversal-close of a **long** fires on a
//! **short** (bearish, upper-wick) signal, and vice-versa:
//!
//! - **Short** (bearish, upper-wick rejection): `body_top + (high - body_top) / 2`
//! - **Long**  (bullish, lower-wick rejection): `body_bot - (body_bot - low) / 2`
//!
//! computed on the **print** bar (`body_top = max(open, close)`,
//! `body_bot = min(open, close)`).
//!
//! # Replay == live
//!
//! The anchor is a function of *two* bars for engulfers, so it is computed once
//! in the detector ([`crate::signals::detect`]) — where both the first and print
//! bars are in scope — and carried as a scalar on
//! [`SignalGeometry`](crate::signals::SignalGeometry) →
//! [`LatchedSignal`](crate::signals::LatchedSignal) →
//! [`Shell`](crate::intent::Shell). Both consumers read that stored scalar, so
//! the engine (replay) and the live worker can't drift
//! (`[[strategy_changes_in_both_replayer_and_worker]]`):
//!
//! - **Engine** (`engine/src/evaluate.rs::close_windows_pass`) reads
//!   `sig.band_anchor`.
//! - **Worker** (`core/src/dispatch/close.rs::run_close`) reads
//!   `shell.band_anchor` (folded on by `Shell::from_candle_and_signal`).

use crate::broker::Candle;
use crate::intent::{Direction, SignalKind};

/// Compute the band anchor for a detected signal from its **origin** bar (the
/// pattern's earliest covered bar — bar `N-1` for a 2-bar engulfer, the print
/// bar itself for a 1-bar pinbar) and its **print** bar (bar `N`).
///
/// `dir` is the **signal** direction (the direction of the reversal candle) —
/// for a long-position close that is `Short`, for a short-position close it is
/// `Long`. Called once in the detector; the result is carried as a scalar.
pub fn band_anchor(kind: SignalKind, dir: Direction, origin: &Candle, print: &Candle) -> f64 {
    match kind {
        // Engulfers: the FIRST bar's open — where the pair sat at/into the level
        // before engulfing back out of the zone.
        SignalKind::RegularEngulfer | SignalKind::FloatingEngulfer => origin.o,
        // Wick-rejection patterns: the midpoint of the print bar's rejection
        // wick must merge with the band. The wick runs from the body edge to the
        // extreme.
        SignalKind::Pinbar | SignalKind::Tweezer | SignalKind::DoubleTweezer => {
            let body_top = print.o.max(print.c);
            let body_bot = print.o.min(print.c);
            match dir {
                // Bearish reversal: long upper wick from body_top up to the high.
                Direction::Short => body_top + (print.h - body_top) / 2.0,
                // Bullish reversal: long lower wick from body_bot down to the low.
                Direction::Long => body_bot - (body_bot - print.l) / 2.0,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn ts() -> DateTime<Utc> {
        "2026-05-26T10:00:00Z".parse().unwrap()
    }

    fn c(o: f64, h: f64, l: f64, cl: f64) -> Candle {
        Candle {
            time: ts(),
            o,
            h,
            l,
            c: cl,
        }
    }

    #[test]
    fn engulfer_anchors_on_first_bar_open() {
        // origin (1st bar) open = 10545.0; print (2nd bar) open = 10551.7.
        // The anchor is the FIRST bar's open, not the print bar's.
        let origin = c(10545.0, 10553.0, 10540.0, 10552.0);
        let print = c(10551.7, 10559.7, 10532.1, 10532.9);
        for kind in [SignalKind::RegularEngulfer, SignalKind::FloatingEngulfer] {
            let a = band_anchor(kind, Direction::Short, &origin, &print);
            assert!((a - 10545.0).abs() < 1e-9, "kind {kind:?}");
        }
    }

    #[test]
    fn short_pinbar_anchors_on_print_upper_wick_midpoint() {
        // Bearish pinbar: body at the bottom, long upper wick on the PRINT bar.
        // print open=10, close=11, high=20 → body_top=11, mid = 11 + (20-11)/2 = 15.5.
        // For a 1-bar pinbar origin == print, so origin is ignored anyway.
        let print = c(10.0, 20.0, 9.5, 11.0);
        let a = band_anchor(SignalKind::Pinbar, Direction::Short, &print, &print);
        assert!((a - 15.5).abs() < 1e-9);
    }

    #[test]
    fn long_pinbar_anchors_on_print_lower_wick_midpoint() {
        // print open=19, close=18, low=10 → body_bot=18, mid = 18 - (18-10)/2 = 14.
        let print = c(19.0, 20.5, 10.0, 18.0);
        let a = band_anchor(SignalKind::Pinbar, Direction::Long, &print, &print);
        assert!((a - 14.0).abs() < 1e-9);
    }

    #[test]
    fn tweezer_and_double_key_off_print_wick_not_origin_open() {
        // Even though tweezers span 2-3 bars, the anchor is the print bar's
        // wick-50%, NOT the origin open — they're wick-rejection patterns.
        let origin = c(999.0, 999.0, 999.0, 999.0); // absurd, to prove it's unused
        let print = c(10.0, 20.0, 9.5, 11.0);
        for kind in [SignalKind::Tweezer, SignalKind::DoubleTweezer] {
            let a = band_anchor(kind, Direction::Short, &origin, &print);
            assert!((a - 15.5).abs() < 1e-9, "kind {kind:?}");
        }
    }
}
