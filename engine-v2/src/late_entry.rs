//! [`resolve`] — **late-entry parity** for a placement the engine produced on a
//! *backlog* bar (a bar the worker missed while it was down and is now catching
//! up on).
//!
//! # The principle: parity with "we were never down"
//!
//! When the worker reboots after downtime it replays the bars it missed. If an
//! enter's preconditions became satisfied on one of those *backlog* bars, we must
//! NOT blindly place the order now — that would place an order for a signal hours
//! stale, at a stale price. Nor do we invent chase heuristics (thresholds, margins,
//! re-placing moved limits). Instead we ask one question:
//!
//! > *If we had been running the whole time, what would this order have done
//! > between the bar it was placed on and now?*
//!
//! and land in exactly one of two states:
//!
//! - [`LateEntry::Missed`] — the order **would have triggered** somewhere in the
//!   gap. In the counterfactual we'd now be mid-trade (or already stopped / taken
//!   profit), which we can't reconstruct or manage. So we don't place; we record
//!   the entry as missed. (A **market** order is *always* missed off its own bar —
//!   an instantaneous fill at a past instant can't be reproduced now.)
//! - [`LateEntry::PlaceLate`] — the order **would still be resting** (it never
//!   triggered across the gap) **and** at the live bar price is still on the side
//!   that would let it trigger later. Placing it now — unchanged, at its original
//!   trigger — is exact parity: it would just be sitting there waiting, so we sit
//!   and wait too.
//!
//! Anything not cleanly "still resting and still valid" collapses to `Missed`.
//!
//! # Fill semantics (matches the replay harness)
//!
//! A resting order's trigger, per [`EntrySpec`](trade_control_core::intent::EntrySpec):
//!
//! - **Stop** — triggers when price trades *through* the level. A long stop sits
//!   *above* price and fills when a bar's **high ≥ trigger**; a short stop sits
//!   *below* and fills when a bar's **low ≤ trigger**.
//! - **Limit** — fills when price comes *back* to the level. A long limit sits
//!   *below* price and fills when a bar's **low ≤ trigger**; a short limit sits
//!   *above* and fills when a bar's **high ≥ trigger**.
//! - **Market** — fills instantly on its own bar; never "rests".
//!
//! The gap window the caller passes is `(placement_bar, live_bar]` — the bars
//! strictly after the bar that would have placed the order, up to and including
//! the live bar. The placement bar itself is excluded (the order isn't resting
//! *during* the bar that creates it — see `[[replay_fill_skips_fire_bar]]`).

use trade_control_core::broker::Candle;
use trade_control_core::intent::Direction;

use crate::plan::EntryMechanism;

/// The minimal order shape the parity simulator needs: how it would rest, which
/// way it trades, and its resolved trigger price. Deliberately decoupled from the
/// full [`EntrySpec`](trade_control_core::intent::EntrySpec) / resolution
/// machinery — trigger *resolution* (anchors, ATR, pips) is the executor's job;
/// this module only replays an already-resolved trigger against candles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LateEntryOrder {
    /// Stop / limit / market — how the order rests (or, for market, that it
    /// doesn't).
    pub mechanism: EntryMechanism,
    /// Trade direction — sets which side of the trigger fills.
    pub direction: Direction,
    /// The resolved trigger price. `None` for [`EntryMechanism::Market`] (a
    /// market order has no resting trigger); `Some` for stop / limit.
    pub trigger: Option<f64>,
}

/// The parity verdict for a backlog-bar placement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LateEntry {
    /// The order would have triggered during the gap (or is a market order): the
    /// counterfactual trade already happened and can't be reconstructed — record
    /// it as missed, place nothing.
    Missed,
    /// The order would still be resting and is still valid at the live bar: place
    /// it now, unchanged, at its original trigger.
    PlaceLate,
}

/// Resolve a backlog-bar placement to [`Missed`](LateEntry::Missed) or
/// [`PlaceLate`](LateEntry::PlaceLate), by replaying `order` against the `gap`
/// candles (the bars strictly after the placement bar, through the live bar).
///
/// - `order` — the resolved order (mechanism + direction + trigger).
/// - `gap` — the gap window `(placement_bar, live_bar]`, ascending. The **last**
///   element is the live bar, whose close is the "current price" the still-valid
///   side check uses. An **empty** gap means there is no bar between placement and
///   now to fill against *and* no live bar to place on — treated as `Missed`
///   (there is nothing to place late onto; the caller should not have routed a
///   live-bar placement here).
///
/// Market ⇒ always [`Missed`]. Stop/limit ⇒ [`Missed`] if it would have triggered
/// anywhere in `gap`, else [`PlaceLate`] iff the live-bar close is still on the
/// resting side of the trigger.
pub fn resolve(order: &LateEntryOrder, gap: &[Candle]) -> LateEntry {
    // A market order can never be placed late — its counterfactual fill was an
    // instantaneous event at a past bar.
    if order.mechanism == EntryMechanism::Market {
        return LateEntry::Missed;
    }

    // Stop / limit both need a resolved trigger. A missing trigger is a
    // mis-constructed order; treat conservatively as missed rather than guessing.
    let Some(trigger) = order.trigger else {
        return LateEntry::Missed;
    };

    // The live bar is the last of the gap; its close is "current price". No live
    // bar ⇒ nothing to place onto ⇒ missed.
    let Some(live) = gap.last() else {
        return LateEntry::Missed;
    };

    // Would the order have triggered on ANY gap bar? If so the counterfactual
    // trade already played out — missed.
    if gap
        .iter()
        .any(|c| would_trigger(order.mechanism, order.direction, trigger, c))
    {
        return LateEntry::Missed;
    }

    // It never triggered. Place late iff the live close is still on the resting
    // side of the trigger — i.e. the order could still trigger *later* from here.
    // (Otherwise price has run past the trigger the "wrong" way and a fresh
    // placement wouldn't behave like the original resting order.)
    if still_valid_side(order.mechanism, order.direction, trigger, live.c) {
        LateEntry::PlaceLate
    } else {
        LateEntry::Missed
    }
}

/// Would a resting `mechanism`/`direction` order with `trigger` fill on `candle`?
///
/// Stop long: high ≥ trigger. Stop short: low ≤ trigger. Limit long: low ≤
/// trigger. Limit short: high ≥ trigger. (Market never rests — handled by the
/// caller; here it conservatively returns `false`.)
fn would_trigger(
    mechanism: EntryMechanism,
    direction: Direction,
    trigger: f64,
    candle: &Candle,
) -> bool {
    match (mechanism, direction) {
        // Stop breaks *through* the level.
        (EntryMechanism::Stop, Direction::Long) => candle.h >= trigger,
        (EntryMechanism::Stop, Direction::Short) => candle.l <= trigger,
        // Limit is a pullback *back to* the level.
        (EntryMechanism::Limit, Direction::Long) => candle.l <= trigger,
        (EntryMechanism::Limit, Direction::Short) => candle.h >= trigger,
        // Market has no resting trigger.
        (EntryMechanism::Market, _) => false,
    }
}

/// Is `price` (the live bar's close) still on the side of `trigger` from which the
/// order could trigger *later* — i.e. would a fresh placement behave like the
/// original resting order rather than fill instantly / never?
///
/// Long stop rests *above* price → still valid while `price < trigger`.
/// Short stop rests *below* → `price > trigger`.
/// Long limit rests *below* price → `price > trigger`.
/// Short limit rests *above* → `price < trigger`.
///
/// Equality (price exactly at the trigger) counts as **not** still-resting — the
/// order would fill immediately, which is not parity with a resting order, so it
/// falls to `Missed`.
fn still_valid_side(
    mechanism: EntryMechanism,
    direction: Direction,
    trigger: f64,
    price: f64,
) -> bool {
    match (mechanism, direction) {
        (EntryMechanism::Stop, Direction::Long) => price < trigger,
        (EntryMechanism::Stop, Direction::Short) => price > trigger,
        (EntryMechanism::Limit, Direction::Long) => price > trigger,
        (EntryMechanism::Limit, Direction::Short) => price < trigger,
        (EntryMechanism::Market, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};

    fn t(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 2, h, 0, 0)
            .single()
            .expect("valid")
    }

    fn c(h: u32, high: f64, low: f64, close: f64) -> Candle {
        Candle {
            time: t(h),
            o: close,
            h: high,
            l: low,
            c: close,
        }
    }

    fn stop_long(trigger: f64) -> LateEntryOrder {
        LateEntryOrder {
            mechanism: EntryMechanism::Stop,
            direction: Direction::Long,
            trigger: Some(trigger),
        }
    }

    fn limit_long(trigger: f64) -> LateEntryOrder {
        LateEntryOrder {
            mechanism: EntryMechanism::Limit,
            direction: Direction::Long,
            trigger: Some(trigger),
        }
    }

    // --- Market ---------------------------------------------------------------

    #[test]
    fn market_is_always_missed() {
        let order = LateEntryOrder {
            mechanism: EntryMechanism::Market,
            direction: Direction::Long,
            trigger: None,
        };
        assert_eq!(
            resolve(&order, &[c(12, 1.11, 1.09, 1.10)]),
            LateEntry::Missed
        );
    }

    // --- Stop (long) ----------------------------------------------------------

    #[test]
    fn long_stop_that_would_have_triggered_is_missed() {
        // Trigger 1.1050. A gap bar's high reaches 1.1060 → would have filled.
        let gap = [
            c(12, 1.1040, 1.1030, 1.1035),
            c(13, 1.1060, 1.1045, 1.1055), // high 1.1060 >= 1.1050
        ];
        assert_eq!(resolve(&stop_long(1.1050), &gap), LateEntry::Missed);
    }

    #[test]
    fn long_stop_still_resting_below_trigger_places_late() {
        // Never reached 1.1050; live close 1.1035 still below → still resting.
        let gap = [c(12, 1.1040, 1.1030, 1.1035), c(13, 1.1045, 1.1032, 1.1035)];
        assert_eq!(resolve(&stop_long(1.1050), &gap), LateEntry::PlaceLate);
    }

    // (No "stop never-triggered but price now above" test: for a stop, a close
    // above the trigger is physically unreachable without the bar's high touching
    // the trigger first — so that path always resolves via `would_trigger`, not
    // `still_valid_side`. The limit "ran below" test below and the
    // `price_exactly_at_trigger` edge case exercise the `still_valid_side` branch.)

    // --- Limit (long) ---------------------------------------------------------

    #[test]
    fn long_limit_that_would_have_filled_is_missed() {
        // Long limit at 1.1000; a gap bar dips to 1.0995 → would have filled.
        let gap = [
            c(12, 1.1020, 1.1005, 1.1010),
            c(13, 1.1015, 1.0995, 1.1008), // low 1.0995 <= 1.1000
        ];
        assert_eq!(resolve(&limit_long(1.1000), &gap), LateEntry::Missed);
    }

    #[test]
    fn long_limit_still_resting_above_trigger_places_late() {
        // Never dipped to 1.1000; live close 1.1010 still above → still resting.
        let gap = [c(12, 1.1020, 1.1005, 1.1010), c(13, 1.1018, 1.1006, 1.1010)];
        assert_eq!(resolve(&limit_long(1.1000), &gap), LateEntry::PlaceLate);
    }

    #[test]
    fn long_limit_price_ran_below_without_a_wick_touch_is_missed() {
        // No bar's low reached 1.1000 (lows stay >= 1.1002), but the live close is
        // 1.0999 — price has run *below* the limit. It's no longer "resting above"
        // → not still-valid → missed. (Physically this needs the down-move to
        // happen via closes, e.g. a fast gap; we model it with lows just above the
        // trigger and a close below.)
        let gap = [
            c(12, 1.1020, 1.1005, 1.1010),
            Candle {
                time: t(13),
                o: 1.1004,
                h: 1.1004,
                l: 1.1002, // low never reached 1.1000 → did not trigger
                c: 1.0999, // but closed below the trigger
            },
        ];
        assert_eq!(resolve(&limit_long(1.1000), &gap), LateEntry::Missed);
    }

    // --- Edge cases -----------------------------------------------------------

    #[test]
    fn empty_gap_is_missed() {
        assert_eq!(resolve(&stop_long(1.1050), &[]), LateEntry::Missed);
    }

    #[test]
    fn missing_trigger_is_missed() {
        let order = LateEntryOrder {
            mechanism: EntryMechanism::Stop,
            direction: Direction::Long,
            trigger: None,
        };
        assert_eq!(
            resolve(&order, &[c(12, 1.11, 1.09, 1.10)]),
            LateEntry::Missed
        );
    }

    #[test]
    fn price_exactly_at_trigger_on_live_is_missed_not_placed() {
        // Long stop 1.1050; no bar's high reached it (highs <= 1.1049), but the
        // live close is exactly 1.1050. Equality = would fill instantly = not
        // still-resting → missed.
        let gap = [c(12, 1.1045, 1.1030, 1.1040), c(13, 1.1049, 1.1041, 1.1050)];
        assert_eq!(resolve(&stop_long(1.1050), &gap), LateEntry::Missed);
    }
}
