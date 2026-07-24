//! Pure geometry for the **pullback** prep — an alternative to the retest.
//!
//! A retest asks "did price return to touch the neckline?". A **pullback** asks
//! "did price retrace ≥ N×ATR from its running body-extreme since we armed?".
//! It is independent of any neckline / break: it anchors to **arm time** (when
//! the operator ran `tv-arm` and the plan was signed + uploaded), not to a break
//! or a drawn line, so it works even on a `--skip-break-and-close` plan.
//!
//! ## Definition (for a **long**)
//!
//! - **Anchor** = the mid open of the candle live at arm time, baked onto the
//!   signed plan by `tv-arm` (passed here as `anchor_open`). It is *not*
//!   rediscovered from the window — bake-and-pass keeps replay == live and avoids
//!   ambiguity when the window is short or the arm happened mid-bar.
//! - **Running high** = the highest `max(open, close)` (body, not wick) of every
//!   bar from `armed_at` forward, up to and including the bar under test.
//! - **Pullback fires** when the bar under test has an **open or close** that is
//!   **≥ `atr_mult × ATR` below** that running high.
//!
//! **Short** mirrors it: running *low* = lowest `min(open, close)`; fires when the
//! bar's open or close is ≥ `atr_mult × ATR` **above** it.
//!
//! ## Why bodies, not wicks
//!
//! A liquidity-vacuum wick (e.g. a spread-hour rubbish candle) can neither set a
//! false running extreme nor falsely trigger the pullback. Only settled body
//! prices (open / close) move the extreme or fire the prep. This composes with
//! the engine's existing spread-hour suppression at the call site.
//!
//! ## This module is pure
//!
//! Nothing here touches `PlanState`, `TradePlan`, or I/O — it takes the anchor,
//! the candle window, `armed_at`, the ATR, the multiple, the direction, and the
//! candle under test, and returns a bool. The engine's thin `stamp_pullback`
//! (in `evaluate.rs`) owns ATR resolution, the `pullback_seen_at` latch, and the
//! prep dispatch. Keeping the math here makes it exhaustively unit-testable and
//! makes the later `engine-v2` port a near-copy.

use chrono::{DateTime, Utc};
use trade_control_core::broker::Candle;
use trade_control_core::intent::Direction;

/// The body value of a candle in the pullback's *extreme* direction:
/// `max(open, close)` for a long (we track the running high), `min(open, close)`
/// for a short (the running low).
fn extreme_body(candle: &Candle, dir: Direction) -> f64 {
    match dir {
        Direction::Long => candle.o.max(candle.c),
        Direction::Short => candle.o.min(candle.c),
    }
}

/// The body value of a candle in the pullback's *trigger* direction — the value
/// we compare against the retraced level. A long fires on a body **low**
/// (`min(open, close)` dipping far enough below the running high); a short fires
/// on a body **high**.
fn trigger_body(candle: &Candle, dir: Direction) -> f64 {
    match dir {
        Direction::Long => candle.o.min(candle.c),
        Direction::Short => candle.o.max(candle.c),
    }
}

/// The running body-extreme over bars in `[armed_at, up_to]` (inclusive),
/// looking only at settled bodies (`max`/`min` of open & close). `None` if no
/// bar in the window falls in that range (e.g. the window hasn't reached
/// `armed_at` yet, or `up_to` precedes it).
///
/// For a **long** this is the highest `max(open, close)`; for a **short** the
/// lowest `min(open, close)`.
pub fn body_extreme(
    window: &[Candle],
    armed_at: DateTime<Utc>,
    up_to: DateTime<Utc>,
    dir: Direction,
) -> Option<f64> {
    window
        .iter()
        .filter(|c| c.time >= armed_at && c.time <= up_to)
        .map(|c| extreme_body(c, dir))
        .reduce(|acc, v| match dir {
            Direction::Long => acc.max(v),
            Direction::Short => acc.min(v),
        })
}

/// Has price retraced ≥ `atr_mult × atr` from the running body-extreme since
/// `armed_at`, measured on **this** candle's body?
///
/// The running extreme is computed over `[armed_at, candle.time]` (so the bar
/// under test participates in its own extreme — a bar that both makes the high
/// and retraces within itself can fire). The `anchor_open` (the baked arm-time
/// mid-open) seeds the extreme so a plan that only ever declines still has a
/// reference: for a long the running high is at least `anchor_open`, for a short
/// the running low is at most `anchor_open`.
///
/// Returns `false` (never fires) if `atr` is not finite/positive or the window
/// has no bar in range — the caller treats a cold ATR as "can't evaluate yet".
pub fn triggered(
    anchor_open: f64,
    window: &[Candle],
    armed_at: DateTime<Utc>,
    atr: f64,
    atr_mult: f64,
    dir: Direction,
    candle: &Candle,
) -> bool {
    if !atr.is_finite() || atr <= 0.0 || atr_mult <= 0.0 {
        return false;
    }
    // Seed the running extreme with the baked anchor so a monotonic decline (that
    // never prints a new body-high past the anchor) still measures its retrace
    // from the arm-time open.
    let running = match body_extreme(window, armed_at, candle.time, dir) {
        Some(bars_extreme) => match dir {
            Direction::Long => bars_extreme.max(anchor_open),
            Direction::Short => bars_extreme.min(anchor_open),
        },
        // No bar in range yet — fall back to the anchor alone.
        None => anchor_open,
    };
    let distance = atr_mult * atr;
    let body = trigger_body(candle, dir);
    match dir {
        // Long: fired when the body dips ≥ distance BELOW the running high.
        Direction::Long => body <= running - distance,
        // Short: fired when the body rises ≥ distance ABOVE the running low.
        Direction::Short => body >= running + distance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(secs: i64, o: f64, h: f64, l: f64, c: f64) -> Candle {
        Candle {
            time: DateTime::from_timestamp(secs, 0).unwrap(),
            o,
            h,
            l,
            c,
        }
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn body_extreme_long_is_highest_body_ignoring_wicks() {
        // Bar 2 has the highest WICK (h=20) but a modest body (max(o,c)=11);
        // bar 3's body max(o,c)=13 is the real running high.
        let w = [
            candle(100, 10.0, 10.5, 9.5, 10.2),
            candle(200, 10.2, 20.0, 10.0, 11.0), // huge upper wick, body 11
            candle(300, 12.0, 13.5, 11.8, 13.0), // body 13
        ];
        assert_eq!(
            body_extreme(&w, ts(100), ts(300), Direction::Long),
            Some(13.0)
        );
    }

    #[test]
    fn body_extreme_short_is_lowest_body_ignoring_wicks() {
        let w = [
            candle(100, 10.0, 10.5, 9.5, 9.8),
            candle(200, 9.8, 10.0, 2.0, 9.0), // huge lower wick, body min 9.0
            candle(300, 9.0, 9.2, 8.0, 8.5),  // body min 8.5
        ];
        assert_eq!(
            body_extreme(&w, ts(100), ts(300), Direction::Short),
            Some(8.5)
        );
    }

    #[test]
    fn body_extreme_respects_armed_at_and_up_to_bounds() {
        let w = [
            candle(100, 5.0, 5.0, 5.0, 5.0),     // before armed_at → excluded
            candle(200, 10.0, 10.0, 10.0, 12.0), // in range
            candle(300, 20.0, 20.0, 20.0, 20.0), // after up_to → excluded
        ];
        assert_eq!(
            body_extreme(&w, ts(200), ts(200), Direction::Long),
            Some(12.0)
        );
    }

    #[test]
    fn body_extreme_none_when_no_bar_in_range() {
        let w = [candle(100, 5.0, 5.0, 5.0, 5.0)];
        assert_eq!(body_extreme(&w, ts(500), ts(600), Direction::Long), None);
    }

    #[test]
    fn long_fires_when_body_retraces_one_atr_below_running_high() {
        // Anchor open 100. Bar makes a high (close 110), later bar closes at 99.5
        // — that's 10.5 below the running high of 110, ≥ 1×ATR (10). Fires.
        let w = [
            candle(100, 100.0, 100.0, 100.0, 100.0),
            candle(200, 105.0, 111.0, 104.0, 110.0), // running high body = 110
            candle(300, 108.0, 108.5, 99.0, 99.5),   // body low 99.5
        ];
        let atr = 10.0;
        assert!(triggered(
            100.0,
            &w,
            ts(100),
            atr,
            1.0,
            Direction::Long,
            &w[2],
        ));
    }

    #[test]
    fn long_does_not_fire_on_shallow_retrace() {
        // Same running high 110, but the last bar only pulls back to 105 (5 < 10).
        let w = [
            candle(100, 100.0, 100.0, 100.0, 100.0),
            candle(200, 105.0, 111.0, 104.0, 110.0),
            candle(300, 106.0, 107.0, 105.0, 105.0), // body low 105 → only 5 back
        ];
        assert!(!triggered(
            100.0,
            &w,
            ts(100),
            10.0,
            1.0,
            Direction::Long,
            &w[2],
        ));
    }

    #[test]
    fn long_wick_below_does_not_fire_only_body_counts() {
        // Last bar WICKS to 95 (l=95) but its body low (min(o,c)) is 108 — a wick
        // through the level with a body that held must NOT fire.
        let w = [
            candle(100, 100.0, 100.0, 100.0, 100.0),
            candle(200, 105.0, 111.0, 104.0, 110.0),
            candle(300, 109.0, 110.0, 95.0, 108.0), // wick to 95, body low 108
        ];
        assert!(!triggered(
            100.0,
            &w,
            ts(100),
            10.0,
            1.0,
            Direction::Long,
            &w[2],
        ));
    }

    #[test]
    fn short_fires_when_body_retraces_one_atr_above_running_low() {
        // Anchor 100. Running low body = 90 (bar 2). Last bar's body high 100.5,
        // ≥ 1×ATR(10) above 90 → fires.
        let w = [
            candle(100, 100.0, 100.0, 100.0, 100.0),
            candle(200, 95.0, 96.0, 89.0, 90.0), // running low body 90
            candle(300, 92.0, 101.0, 91.0, 100.5), // body high 100.5
        ];
        assert!(triggered(
            100.0,
            &w,
            ts(100),
            10.0,
            1.0,
            Direction::Short,
            &w[2],
        ));
    }

    #[test]
    fn anchor_seeds_extreme_for_monotonic_decline_long() {
        // Price only ever declines from the anchor (100) — no bar prints a body
        // above 100 — but a bar 11 below the anchor still counts as a 1-ATR
        // pullback measured from the anchor-seeded running high.
        let w = [
            candle(100, 100.0, 100.0, 100.0, 100.0),
            candle(200, 95.0, 95.0, 88.0, 89.0), // body low 89 = 11 below anchor
        ];
        assert!(triggered(
            100.0,
            &w,
            ts(100),
            10.0,
            1.0,
            Direction::Long,
            &w[1],
        ));
    }

    #[test]
    fn atr_multiple_scales_the_threshold() {
        let w = [
            candle(100, 100.0, 100.0, 100.0, 100.0),
            candle(200, 105.0, 111.0, 104.0, 110.0),
            candle(300, 100.0, 101.0, 96.0, 96.0), // 14 below running high 110
        ];
        // 14 back ≥ 1.0×ATR(10) → fires; but < 1.5×ATR(15) → does not.
        assert!(triggered(
            100.0,
            &w,
            ts(100),
            10.0,
            1.0,
            Direction::Long,
            &w[2]
        ));
        assert!(!triggered(
            100.0,
            &w,
            ts(100),
            10.0,
            1.5,
            Direction::Long,
            &w[2]
        ));
    }

    #[test]
    fn cold_or_nonpositive_atr_never_fires() {
        let w = [
            candle(100, 100.0, 100.0, 100.0, 100.0),
            candle(200, 105.0, 111.0, 104.0, 90.0),
        ];
        assert!(!triggered(
            100.0,
            &w,
            ts(100),
            f64::NAN,
            1.0,
            Direction::Long,
            &w[1]
        ));
        assert!(!triggered(
            100.0,
            &w,
            ts(100),
            0.0,
            1.0,
            Direction::Long,
            &w[1]
        ));
        assert!(!triggered(
            100.0,
            &w,
            ts(100),
            10.0,
            0.0,
            Direction::Long,
            &w[1]
        ));
    }
}
