//! Pure fill-side geometry for restoring a resting entry order cancelled
//! during a spread blackout (System 3, Sub-plan 5).
//!
//! When the blackout lifts, the recovery watcher must decide — per cancelled
//! order — whether the order is still worth re-placing. The decision is the
//! sign-bug-prone seam, so it lives here as two pure predicates with a full
//! truth-table of unit tests. NO KV, NO broker, NO time — just price geometry.
//!
//! **Fill-side, not mid.** Spread is a real round-trip cost and must count
//! *against* recreating a deep order — the operator's explicit choice. A long
//! recreates against `ask` (the price you'd pay to BUY); a short against `bid`
//! (the price you'd SELL at). Using mid here would understate the cost of
//! re-entering into a still-wide spread and re-place orders that the operator
//! would rather drop.
//!
//! | entry kind | recreate if… | fill side |
//! |---|---|---|
//! | **Long stop**  | `ask` still **below** `trigger + sl_distance` | buy at `ask` |
//! | **Short stop** | `bid` still **above** `trigger − sl_distance` | sell at `bid` |
//! | **Long limit** | `ask` **between** `entry` and `tp` (above entry, below tp) | buy at `ask` |
//! | **Short limit**| `bid` **between** `entry` and `tp` (below entry, above tp) | sell at `bid` |
//!
//! `sl_distance` is `|trigger − stop_loss|` in price units; bands expressed in
//! pips convert via the intent's baked `pip_size` *before* reaching here (these
//! fns take raw price distances, never pips).

use crate::intent::{Direction, OnTooCloseAction, ResolvedEntry};

/// What the recovery watcher should do with one cancelled resting order, given
/// fresh fill-side bid/ask. The pure decision — no KV, no broker — so the
/// branch logic is unit-tested in `core` and the watcher is a thin wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePlan {
    /// Re-drive the entry through `run_enter` — the order is still placeable as
    /// a stop/limit, OR it's an overrun stop whose `on_too_close` is NOT skip
    /// (so the broker → Sub-plan-0 fallback should get a chance to recover it
    /// as market/limit). One code path; the broker is the authority.
    Redrive,
    /// Stop overrun (fill-side blew past trigger beyond the SL band) AND the
    /// stored intent's `on_too_close` is `skip` (or absent). Short-circuit:
    /// drop without a guaranteed-to-fail broker round-trip.
    DropStopOverrunSkip,
    /// Limit on the wrong side / past TP. Drop it and leave the trade looking
    /// for entry — limits are themselves a fallback, so a stale one is fine to
    /// drop. NEVER routed to the stop `on_too_close` fallback.
    DropStaleLimit,
    /// A resting "market" order shouldn't exist (market entries fill
    /// immediately, never rest). Drop + log; nothing to restore.
    DropUnexpectedMarket,
}

/// Decide the restore action for one cancelled order. Pure: takes the resolved
/// geometry + a fresh fill-side quote + whether the stored intent's
/// `on_too_close` is `skip`, and returns a [`RestorePlan`].
///
/// `sl_distance` for a stop is `|trigger − stop_loss|`; for a limit `tp` is the
/// take-profit and `entry` the trigger. The watcher computes these off the
/// re-parsed `Resolved` (which has the baked `pip_size`) before calling.
pub fn restore_plan(
    entry: &ResolvedEntry,
    direction: Direction,
    stop_loss: f64,
    take_profit: f64,
    bid: f64,
    ask: f64,
    on_too_close: Option<OnTooCloseAction>,
) -> RestorePlan {
    match entry {
        ResolvedEntry::Stop { trigger_price } => {
            let sl_distance = (trigger_price - stop_loss).abs();
            if recreate_stop(direction, *trigger_price, sl_distance, bid, ask) {
                RestorePlan::Redrive
            } else if matches!(on_too_close, None | Some(OnTooCloseAction::Skip)) {
                // Overrun + skip ⇒ no point re-driving (guaranteed to fail then
                // skip); short-circuit. A non-skip fallback re-drives so the
                // broker → Sub-plan-0 path can recover it.
                RestorePlan::DropStopOverrunSkip
            } else {
                RestorePlan::Redrive
            }
        }
        ResolvedEntry::Limit {
            trigger_price: entry_price,
        } => {
            if recreate_limit(direction, *entry_price, take_profit, bid, ask) {
                RestorePlan::Redrive
            } else {
                RestorePlan::DropStaleLimit
            }
        }
        ResolvedEntry::Market { .. } => RestorePlan::DropUnexpectedMarket,
    }
}

/// Recreate a STOP entry only if the fill-side price hasn't blown past the
/// trigger by more than the SL distance — i.e. the move we wanted is still
/// available.
///
/// Returns `false` → DO NOT recreate as a stop → the caller routes to the
/// Sub-plan-0 `on_too_close` fallback (market / limit / skip) encoded on the
/// stored intent. A long buy-stop wants price to break *up* through `trigger`;
/// once `ask` has run all the way to `trigger + sl_distance` the entire planned
/// SL distance is already consumed by the time we'd fill — the edge is gone.
/// The short case mirrors it below `trigger - sl_distance`.
///
/// Boundary: exactly at `trigger ± sl_distance` returns `false` (the strict
/// `<` / `>`) — at the band edge the move is already gone, route to fallback.
pub fn recreate_stop(
    direction: Direction,
    trigger: f64,
    sl_distance: f64,
    bid: f64,
    ask: f64,
) -> bool {
    match direction {
        // Buy-stop: still placeable while the ask hasn't climbed to trigger+band.
        Direction::Long => ask < trigger + sl_distance,
        // Sell-stop: still placeable while the bid hasn't fallen to trigger-band.
        Direction::Short => bid > trigger - sl_distance,
    }
}

/// Recreate a LIMIT entry only if the fill-side price is still on the correct
/// (pullback) side, strictly between `entry` and `tp`.
///
/// Returns `false` → DROP the limit and leave the trade "looking for entry"
/// (limits are themselves a fallback option, so dropping a stale one is fine —
/// it is NEVER routed to the `on_too_close` stop fallback).
///
/// Directional ordering is load-bearing: a long limit has `tp > entry`, a short
/// limit `tp < entry`. The `&&` predicates encode that ordering, so an
/// `entry`/`tp` mix-up surfaces as a failing unit test rather than a silent
/// wrong-side place (see the `swapped_entry_tp_*` tests).
///
/// Boundary: exactly at `entry` or at `tp` returns `false` (strict inequalities)
/// — sitting on the level is not "between" it.
pub fn recreate_limit(direction: Direction, entry: f64, tp: f64, bid: f64, ask: f64) -> bool {
    match direction {
        // Long limit: buy on a pullback above entry but still below tp.
        Direction::Long => ask > entry && ask < tp,
        // Short limit: sell on a pullback below entry but still above tp.
        Direction::Short => bid < entry && bid > tp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- STOP, long -----

    #[test]
    fn long_stop_recreates_when_ask_below_trigger_plus_band() {
        // trigger 1.8000, sl 1.7950 → sl_distance 0.0050. Band top = 1.8050.
        // ask 1.8010 is above trigger but well inside the band → still placeable.
        assert!(recreate_stop(
            Direction::Long,
            1.8000,
            0.0050,
            1.8008,
            1.8010
        ));
        // ask just below trigger (price hasn't even broken yet) → placeable.
        assert!(recreate_stop(
            Direction::Long,
            1.8000,
            0.0050,
            1.7990,
            1.7992
        ));
    }

    #[test]
    fn long_stop_overrun_when_ask_at_or_past_band_top() {
        // ask exactly at trigger+band → overrun (boundary returns false).
        assert!(!recreate_stop(
            Direction::Long,
            1.8000,
            0.0050,
            1.8049,
            1.8050
        ));
        // ask well past band top → overrun → route to on_too_close.
        assert!(!recreate_stop(
            Direction::Long,
            1.8000,
            0.0050,
            1.8090,
            1.8092
        ));
    }

    #[test]
    fn long_stop_just_overrun_returns_false() {
        // One tick past the band edge → false (the on_too_close path).
        assert!(!recreate_stop(
            Direction::Long,
            1.8000,
            0.0050,
            1.80505,
            1.80511
        ));
    }

    // ----- STOP, short -----

    #[test]
    fn short_stop_recreates_when_bid_above_trigger_minus_band() {
        // trigger 1.8000, sl_distance 0.0050. Band bottom = 1.7950.
        // bid 1.7990 above band bottom → still placeable.
        assert!(recreate_stop(
            Direction::Short,
            1.8000,
            0.0050,
            1.7990,
            1.7992
        ));
        // bid above trigger (price not yet broken down) → placeable.
        assert!(recreate_stop(
            Direction::Short,
            1.8000,
            0.0050,
            1.8010,
            1.8012
        ));
    }

    #[test]
    fn short_stop_overrun_when_bid_at_or_past_band_bottom() {
        // bid exactly at trigger-band → overrun (boundary false).
        assert!(!recreate_stop(
            Direction::Short,
            1.8000,
            0.0050,
            1.7950,
            1.7952
        ));
        // bid below band bottom → overrun → on_too_close.
        assert!(!recreate_stop(
            Direction::Short,
            1.8000,
            0.0050,
            1.7900,
            1.7902
        ));
    }

    // ----- LIMIT, long -----

    #[test]
    fn long_limit_recreates_when_ask_between_entry_and_tp() {
        // entry 1.8000, tp 1.8100 (tp > entry for a long). ask 1.8050 between.
        assert!(recreate_limit(
            Direction::Long,
            1.8000,
            1.8100,
            1.8048,
            1.8050
        ));
    }

    #[test]
    fn long_limit_drops_when_ask_below_entry_or_at_tp() {
        // ask below entry → wrong side, drop.
        assert!(!recreate_limit(
            Direction::Long,
            1.8000,
            1.8100,
            1.7989,
            1.7990
        ));
        // ask exactly at entry → not strictly between, drop.
        assert!(!recreate_limit(
            Direction::Long,
            1.8000,
            1.8100,
            1.7998,
            1.8000
        ));
        // ask exactly at tp → not strictly between, drop.
        assert!(!recreate_limit(
            Direction::Long,
            1.8000,
            1.8100,
            1.8099,
            1.8100
        ));
        // ask past tp → drop.
        assert!(!recreate_limit(
            Direction::Long,
            1.8000,
            1.8100,
            1.8149,
            1.8150
        ));
    }

    // ----- LIMIT, short -----

    #[test]
    fn short_limit_recreates_when_bid_between_entry_and_tp() {
        // entry 1.8100, tp 1.8000 (tp < entry for a short). bid 1.8050 between.
        assert!(recreate_limit(
            Direction::Short,
            1.8100,
            1.8000,
            1.8050,
            1.8052
        ));
    }

    #[test]
    fn short_limit_drops_when_bid_above_entry_or_at_tp() {
        // bid above entry → wrong side, drop.
        assert!(!recreate_limit(
            Direction::Short,
            1.8100,
            1.8000,
            1.8150,
            1.8152
        ));
        // bid exactly at entry → not strictly between, drop.
        assert!(!recreate_limit(
            Direction::Short,
            1.8100,
            1.8000,
            1.8100,
            1.8102
        ));
        // bid exactly at tp → not strictly between, drop.
        assert!(!recreate_limit(
            Direction::Short,
            1.8100,
            1.8000,
            1.8000,
            1.8002
        ));
    }

    // ----- swapped entry/tp guard (the sign-bug canary) -----

    #[test]
    fn swapped_entry_tp_long_asserts_false() {
        // Deliberately pass a long limit with tp < entry (operator error /
        // refactor mix-up). The directional ordering must reject it: ask
        // can't be both > entry and < tp when tp < entry → always false.
        assert!(!recreate_limit(
            Direction::Long,
            1.8100, // entry (wrongly higher)
            1.8000, // tp (wrongly lower)
            1.8050,
            1.8052
        ));
    }

    #[test]
    fn swapped_entry_tp_short_asserts_false() {
        // Short limit with tp > entry (swapped). bid can't be < entry and
        // > tp at once when tp > entry → always false.
        assert!(!recreate_limit(
            Direction::Short,
            1.8000, // entry (wrongly lower)
            1.8100, // tp (wrongly higher)
            1.8050,
            1.8052
        ));
    }

    // ----- fill-side discrimination (proves we read ask for long, bid for short) -----

    #[test]
    fn long_stop_reads_ask_not_bid() {
        // A wide spread where bid is inside the band but ask is past it: a
        // long must use ASK, so this is an overrun (false). If the code wrongly
        // used bid it would return true.
        let trigger = 1.8000;
        let sl_distance = 0.0020; // band top 1.8020
        let bid = 1.8015; // inside band
        let ask = 1.8025; // past band top
        assert!(!recreate_stop(
            Direction::Long,
            trigger,
            sl_distance,
            bid,
            ask
        ));
    }

    #[test]
    fn short_stop_reads_bid_not_ask() {
        // Wide spread where ask is inside the band but bid is past it: a short
        // must use BID, so this is an overrun (false).
        let trigger = 1.8000;
        let sl_distance = 0.0020; // band bottom 1.7980
        let bid = 1.7975; // past band bottom
        let ask = 1.7985; // inside band
        assert!(!recreate_stop(
            Direction::Short,
            trigger,
            sl_distance,
            bid,
            ask
        ));
    }

    // ----- restore_plan (the branch-decision seam) -----

    use crate::intent::{OnTooCloseAction, ResolvedEntry};

    #[test]
    fn restore_plan_placeable_stop_redrives() {
        // Long stop still placeable (ask inside band) → re-drive.
        let plan = restore_plan(
            &ResolvedEntry::Stop {
                trigger_price: 1.8000,
            },
            Direction::Long,
            1.7950, // sl → sl_distance 0.0050
            1.8200, // tp (unused for stop)
            1.8008,
            1.8010,
            None,
        );
        assert_eq!(plan, RestorePlan::Redrive);
    }

    #[test]
    fn restore_plan_overrun_stop_with_skip_short_circuits() {
        // Long stop overrun (ask past band top) + on_too_close skip/None →
        // drop without a place call.
        let plan = restore_plan(
            &ResolvedEntry::Stop {
                trigger_price: 1.8000,
            },
            Direction::Long,
            1.7950,
            1.8200,
            1.8090,
            1.8092, // past band top 1.8050
            Some(OnTooCloseAction::Skip),
        );
        assert_eq!(plan, RestorePlan::DropStopOverrunSkip);
        // None behaves like skip.
        let plan_none = restore_plan(
            &ResolvedEntry::Stop {
                trigger_price: 1.8000,
            },
            Direction::Long,
            1.7950,
            1.8200,
            1.8090,
            1.8092,
            None,
        );
        assert_eq!(plan_none, RestorePlan::DropStopOverrunSkip);
    }

    #[test]
    fn restore_plan_overrun_stop_with_market_fallback_redrives() {
        // Overrun, but on_too_close=market → re-drive so the broker →
        // Sub-plan-0 fallback can convert it to a market entry.
        let plan = restore_plan(
            &ResolvedEntry::Stop {
                trigger_price: 1.8000,
            },
            Direction::Long,
            1.7950,
            1.8200,
            1.8090,
            1.8092,
            Some(OnTooCloseAction::Market),
        );
        assert_eq!(plan, RestorePlan::Redrive);
    }

    #[test]
    fn restore_plan_placeable_limit_redrives() {
        // Long limit, ask between entry and tp → re-drive.
        let plan = restore_plan(
            &ResolvedEntry::Limit {
                trigger_price: 1.8000,
            },
            Direction::Long,
            1.7900, // sl (unused for the limit predicate)
            1.8100, // tp
            1.8048,
            1.8050,
            None,
        );
        assert_eq!(plan, RestorePlan::Redrive);
    }

    #[test]
    fn restore_plan_stale_limit_drops() {
        // Long limit, ask below entry → stale, drop (never to on_too_close).
        let plan = restore_plan(
            &ResolvedEntry::Limit {
                trigger_price: 1.8000,
            },
            Direction::Long,
            1.7900,
            1.8100,
            1.7989,
            1.7990,
            // Even if a stray on_too_close were present, a limit is never
            // routed to the stop fallback.
            Some(OnTooCloseAction::Market),
        );
        assert_eq!(plan, RestorePlan::DropStaleLimit);
    }

    #[test]
    fn restore_plan_unexpected_market_drops() {
        let plan = restore_plan(
            &ResolvedEntry::Market {
                reference_price: 1.8000,
            },
            Direction::Long,
            1.7900,
            1.8100,
            1.8000,
            1.8002,
            None,
        );
        assert_eq!(plan, RestorePlan::DropUnexpectedMarket);
    }
}
