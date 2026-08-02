//! *"What should we do with this **resting** order right now?"* — pure.
//!
//! The Pending column of the vocabulary table: placed with the broker, not yet
//! triggered, **not at risk**. That last part is what makes this state different
//! from Live and is the whole reason it gets its own decision function.
//!
//! # Why Pending is not just "Live with a different id"
//!
//! [`sl_target`](super::sl_target) answers *what the stop should be*, identically
//! for every state. What differs is **what may be done about the answer**:
//!
//! | | Live (a position) | Pending (a resting order) |
//! |---|---|---|
//! | widen the stop | amend in place | amend in place |
//! | shrink the stop | **only when in profit** — else it realises a loss | always safe: nothing is at risk yet |
//! | re-size the stake | **never** — a partial close, a real fill with real cost | **required** — else risk drifts off 1% |
//! | falls below `min_r` | ride it out; the trade is already on | **demote to Stored** — un-place it, keep the setup |
//!
//! Two of those four rows invert between the states, so folding Pending into the
//! live path would need a boolean at every branch — which is how the pre-existing
//! widen implementations drifted (see [`super::sl_target`]'s docs). One decision
//! per state, one shared target function underneath.
//!
//! ## Why shrink needs no `in_profit` gate here
//!
//! [`in_profit`](super::in_profit) exists because tightening a *losing position's*
//! stop converts an unrealised loss into a realised one at a level nobody chose.
//! A resting order has no P&L to realise — it has not filled. So the filter is
//! deliberately **absent** from this module rather than merely unused, and the
//! test `shrink_needs_no_profit_gate_when_nothing_is_at_risk` pins that as intent
//! rather than an omission a future reader might "fix".
//!
//! ## Why re-size is required, not optional
//!
//! Stake is derived from the stop distance: `stake = risk / sl_distance`. Move
//! the stop without moving the stake and the trade's risk moves with it — a stop
//! widened 3× at the original stake is a 3% loss on a 1% trade. That is the
//! failure this module exists to prevent, and it is why [`PendingAction::Adjust`]
//! carries a stake and not just a price.
//!
//! # Cancel-and-replace, and the gap it opens
//!
//! The `Broker` trait has **no resize method** — `amend_stop` moves a stop and
//! explicitly leaves the stake untouched. So an adjustment that must also re-size
//! is a *cancel then place*, and between those two calls the order is **not at
//! the broker**. If price reaches the trigger inside that window, the entry is
//! simply missed.
//!
//! This is a deliberate trade, chosen by the operator over the alternative of
//! amending the stop alone and letting risk drift: a missed entry costs an
//! opportunity, wrong-sized risk costs money. The window is one round-trip on a
//! ~5s cron, and the setup re-fires if it is still valid.
//!
//! The re-place can also be **rejected** — price may have drifted past the
//! trigger, which the broker reports as "#19-10 too close to market". That is
//! where the stop↔limit flip belongs, and it already exists on the shared
//! re-place path (`place_entry_too_close_fallback`), so this module deliberately
//! does not grow a second copy of it: it hands the decision back through
//! `run_enter` and inherits the flip.
//!
//! # What this module is not
//!
//! It does not talk to a broker or a store — those live in
//! [`reprice`](super::reprice), the effectful half, exactly as
//! [`stored`](super::stored) is to [`park`](super::park). Keeping the decision
//! pure is what makes it mutation-testable: flip a sign or drop the re-size and a
//! test goes red.

use super::sl_target::{SlAction, SlTarget};

/// How risk-per-trade is expressed, so [`stake_for`] can size an order without
/// caring which the operator used.
///
/// Both resolve to the same thing — an amount of account currency at risk — but
/// they resolve at *different times*: a percentage needs the balance at sizing
/// time, an absolute amount does not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RiskBudget {
    /// Account currency at risk if the stop is hit.
    pub amount: f64,
}

impl RiskBudget {
    /// A fixed cash risk (`risk_amount` on the intent).
    pub fn absolute(amount: f64) -> Self {
        Self { amount }
    }

    /// A percentage of account equity (`risk_pct`), resolved against `balance`.
    pub fn pct_of(risk_pct: f64, balance: f64) -> Self {
        Self {
            amount: balance * risk_pct / 100.0,
        }
    }
}

/// The stake that keeps `budget` at risk over `sl_distance`.
///
/// This is the identity the whole module protects: **risk = stake ×
/// sl_distance**, so moving one without the other moves the risk. A degenerate
/// distance yields `None` rather than an infinite stake — "unjudgeable" must
/// never resolve to "unbounded size".
pub fn stake_for(budget: RiskBudget, sl_distance: f64) -> Option<f64> {
    if !sl_distance.is_finite() || sl_distance <= 0.0 {
        return None;
    }
    if !budget.amount.is_finite() || budget.amount <= 0.0 {
        return None;
    }
    Some(budget.amount / sl_distance)
}

/// What to do with a resting order this candle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PendingAction {
    /// Leave it exactly as it rests. The overwhelmingly common answer.
    Hold,
    /// Cancel and re-place with this stop distance and this stake.
    ///
    /// Both fields move together **by construction** — there is no way to
    /// express "new stop, old stake", which is the bug this type exists to make
    /// unrepresentable.
    Adjust {
        /// The stop distance the replacement should carry, in price units.
        sl_distance: f64,
        /// The stake that keeps risk constant over `sl_distance`.
        stake: f64,
    },
    /// The order can no longer clear its R-floor: pull it from the broker but
    /// **keep the setup** as a [`StoredOrder`](super::StoredOrder), to be
    /// re-placed if the spread calms before expiry.
    ///
    /// Distinct from `Hold`: leaving a sub-1R order resting means it can *fill*
    /// at a risk:reward the operator never accepted.
    Demote,
}

/// Prices/stakes within this fraction of each other count as equal. Without it,
/// float round-tripping would produce an endless cancel-and-replace loop on a
/// stop that never actually moved — every cron tick, on every resting order.
const RELATIVE_EPSILON: f64 = 1e-9;

fn same(a: f64, b: f64) -> bool {
    let scale = a.abs().max(b.abs()).max(1.0);
    (a - b).abs() <= RELATIVE_EPSILON * scale
}

/// Decide what happens to a resting order, given the stop target already
/// computed for it.
///
/// `target` comes from [`sl_target`](super::sl_target) — the same function the
/// live and stored paths use, so there is one notion of what a stop should be.
/// This adds only the part that is specific to *resting* orders.
///
/// # Arguments
///
/// - `target` — the shared verdict, computed against the current spread.
/// - `current_sl_distance` — the stop distance the resting order carries now.
/// - `budget` — risk per trade, for the re-size.
///
/// # Why no `in_profit` parameter
///
/// See the module docs: a resting order has nothing to realise, so shrinking it
/// is unconditionally safe. Adding the parameter "for symmetry" with the live
/// path would import a gate whose justification does not hold here.
pub fn pending_action(
    target: SlTarget,
    current_sl_distance: f64,
    budget: RiskBudget,
) -> PendingAction {
    // Below the R-floor the order must not be left where it can fill at a
    // risk:reward the operator never accepted. Checked FIRST: a sub-1R order is
    // demoted whatever direction its stop would otherwise have moved.
    if target.action == SlAction::BelowMinR {
        return PendingAction::Demote;
    }

    // `Hold` from the shared target means the stop is already right. Note this
    // also covers the unjudgeable-geometry case, which `sl_target` reports as
    // `Hold` — never a reason to start cancelling live orders.
    if target.action == SlAction::Hold {
        return PendingAction::Hold;
    }

    let desired = target.desired_sl_distance;
    // Defence in depth: `sl_target` should not report Widen/Shrink with an
    // unusable distance, but a cancel-and-replace is destructive enough that it
    // must not be reachable through a NaN.
    if !desired.is_finite() || desired <= 0.0 {
        return PendingAction::Hold;
    }
    // A move too small to matter is not worth an unguarded gap at the broker.
    if same(desired, current_sl_distance) {
        return PendingAction::Hold;
    }

    // Re-size or don't move at all. A stop we cannot size is a stop we must not
    // adjust — moving it alone would silently change the trade's risk, which is
    // the exact failure this module exists to prevent.
    match stake_for(budget, desired) {
        Some(stake) => PendingAction::Adjust {
            sl_distance: desired,
            stake,
        },
        None => PendingAction::Hold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order_control::sl_target::{SpreadInputs, sl_target};

    fn budget() -> RiskBudget {
        // $100 at risk — a 1% risk on a $10k account.
        RiskBudget::absolute(100.0)
    }

    fn target(action: SlAction, desired: f64) -> SlTarget {
        SlTarget {
            desired_sl_distance: desired,
            r: 2.0,
            action,
        }
    }

    // ---- the risk identity --------------------------------------------------

    /// The identity the module protects: stake × sl_distance == risk. If this
    /// is wrong, every re-size is wrong.
    ///
    /// Mutation check: change `budget.amount / sl_distance` to a multiply and
    /// this goes red.
    #[test]
    fn stake_keeps_risk_constant_across_stop_distances() {
        for sl in [0.0010, 0.0025, 0.0064, 0.0200] {
            let stake = stake_for(budget(), sl).expect("sizeable");
            assert!(
                (stake * sl - 100.0).abs() < 1e-9,
                "sl={sl} stake={stake} risks {}, not 100",
                stake * sl,
            );
        }
    }

    /// A wider stop must take a SMALLER stake. Stated separately from the
    /// identity because it is the direction a reader checks by eye, and an
    /// inverted formula could still satisfy a single-point test.
    #[test]
    fn a_wider_stop_takes_a_smaller_stake() {
        let tight = stake_for(budget(), 0.0020).expect("sizeable");
        let wide = stake_for(budget(), 0.0064).expect("sizeable");
        assert!(
            wide < tight,
            "widening the stop must shrink the stake: {wide} vs {tight}",
        );
    }

    /// An unjudgeable distance must not resolve to an unbounded position.
    #[test]
    fn degenerate_inputs_are_unsizeable_not_infinite() {
        for bad in [0.0, -0.0020, f64::NAN, f64::INFINITY] {
            assert_eq!(stake_for(budget(), bad), None, "sl_distance={bad}");
        }
        for bad in [0.0, -100.0, f64::NAN] {
            assert_eq!(
                stake_for(RiskBudget::absolute(bad), 0.0020),
                None,
                "risk={bad}",
            );
        }
    }

    #[test]
    fn pct_and_absolute_agree_on_the_same_risk() {
        let pct = RiskBudget::pct_of(1.0, 10_000.0);
        assert!((pct.amount - 100.0).abs() < 1e-9);
    }

    // ---- the decision -------------------------------------------------------

    /// The headline: a widen on a resting order re-sizes. Moving the stop
    /// alone would turn a 1% trade into a 3.2% one.
    ///
    /// Mutation check: return `Adjust { sl_distance, stake: <the old stake> }`
    /// and this goes red.
    #[test]
    fn widening_a_resting_order_resizes_it() {
        let out = pending_action(target(SlAction::Widen, 0.0064), 0.0020, budget());
        let PendingAction::Adjust { sl_distance, stake } = out else {
            panic!("expected an adjust, got {out:?}");
        };
        assert!((sl_distance - 0.0064).abs() < 1e-12);
        assert!(
            (stake * sl_distance - 100.0).abs() < 1e-9,
            "risk must stay at 100, got {}",
            stake * sl_distance,
        );
        // And concretely: the stake fell because the stop got wider.
        let before = stake_for(budget(), 0.0020).expect("sizeable");
        assert!(stake < before);
    }

    /// A resting order shrinks with NO profit gate — nothing is at risk, so
    /// there is no loss to realise. This is the row that inverts against the
    /// live path, pinned so nobody "restores symmetry" by adding the filter.
    #[test]
    fn shrink_needs_no_profit_gate_when_nothing_is_at_risk() {
        let out = pending_action(target(SlAction::Shrink, 0.0020), 0.0064, budget());
        let PendingAction::Adjust { sl_distance, stake } = out else {
            panic!("a resting order shrinks unconditionally, got {out:?}");
        };
        assert!((sl_distance - 0.0020).abs() < 1e-12);
        assert!(
            stake > stake_for(budget(), 0.0064).expect("sizeable"),
            "a tighter stop takes a bigger stake at constant risk",
        );
    }

    /// Below the R-floor a resting order is PULLED, not left to fill at a
    /// risk:reward the operator never accepted — and not discarded either.
    ///
    /// Mutation check: return `Hold` here and this goes red.
    #[test]
    fn sub_min_r_demotes_rather_than_resting_or_dropping() {
        let out = pending_action(target(SlAction::BelowMinR, 0.0100), 0.0020, budget());
        assert_eq!(out, PendingAction::Demote);
    }

    /// The demote check comes FIRST: a sub-1R order is pulled whichever way its
    /// stop would have moved. Ordering these the other way would leave a
    /// sub-1R order resting whenever its target happened to be a shrink.
    #[test]
    fn demote_wins_over_any_stop_movement() {
        // BelowMinR while the desired distance is TIGHTER than current — the
        // arm that a shrink-first ordering would swallow.
        let out = pending_action(target(SlAction::BelowMinR, 0.0010), 0.0500, budget());
        assert_eq!(out, PendingAction::Demote);
    }

    /// The common case, and the one that matters for broker load: an unchanged
    /// stop must not produce a cancel-and-replace.
    #[test]
    fn an_unchanged_stop_holds() {
        let out = pending_action(target(SlAction::Hold, 0.0020), 0.0020, budget());
        assert_eq!(out, PendingAction::Hold);
    }

    /// Float round-tripping must not manufacture an adjust. Without the
    /// epsilon this is an endless cancel-and-replace loop — every tick, on
    /// every resting order, each one opening an unguarded gap.
    ///
    /// Mutation check: replace `same()` with `==` and this goes red.
    #[test]
    fn a_sub_epsilon_move_is_not_worth_an_unguarded_gap() {
        let current = 0.0020;
        let jittered = current * (1.0 + 1e-12);
        let out = pending_action(target(SlAction::Widen, jittered), current, budget());
        assert_eq!(
            out,
            PendingAction::Hold,
            "a move of 1 part in 10^12 must not cancel a live order",
        );
    }

    /// A stop we cannot size is a stop we must not move: adjusting it alone
    /// would silently change the trade's risk.
    #[test]
    fn an_unsizeable_adjust_holds_rather_than_moving_the_stop_alone() {
        let out = pending_action(
            target(SlAction::Widen, 0.0064),
            0.0020,
            RiskBudget::absolute(f64::NAN),
        );
        assert_eq!(
            out,
            PendingAction::Hold,
            "no stake ⇒ no move; never move the stop without the size",
        );
    }

    /// Defence in depth: a NaN target must not reach the cancel path.
    #[test]
    fn a_non_finite_target_never_cancels() {
        for bad in [f64::NAN, 0.0, -0.0020] {
            let out = pending_action(target(SlAction::Widen, bad), 0.0020, budget());
            assert_eq!(out, PendingAction::Hold, "desired={bad}");
        }
    }

    // ---- end-to-end against the real sl_target ------------------------------

    /// The whole point, driven through the *real* target function rather than a
    /// hand-built one: a spike forecast for the coming hour re-sizes a resting
    /// order **before** the spike lands.
    ///
    /// This is the 20:55-fills-at-21:05 case with the order already at the
    /// broker — the state where the old code could only watch.
    #[test]
    fn a_forecast_spike_resizes_a_resting_order_before_it_arrives() {
        let spreads = SpreadInputs {
            last_candle: 0.00015,
            expected_this_hour: 0.00015,
            expected_next_hour: 0.00064,
        };
        let t = sl_target(spreads, 0.0020, 0.0020, 0.0200, 1.0);
        let out = pending_action(t, 0.0020, budget());
        let PendingAction::Adjust { sl_distance, stake } = out else {
            panic!("the forecast must widen a resting order, got {out:?}");
        };
        assert!(
            (sl_distance - 0.0064).abs() < 1e-12,
            "sized off the forecast, not the calm measurement",
        );
        assert!((stake * sl_distance - 100.0).abs() < 1e-9);
    }

    /// ...and the tight scalp that cannot carry the coming spread is demoted
    /// rather than left resting into it.
    #[test]
    fn a_scalp_that_cannot_carry_the_coming_spread_is_demoted() {
        let spiky = SpreadInputs {
            last_candle: 0.00015,
            expected_this_hour: 0.00064,
            expected_next_hour: 0.00064,
        };
        // 20-pip stop, 30-pip TP: fine now, sub-1R once the floor lifts.
        let t = sl_target(spiky, 0.0020, 0.0020, 0.0030, 1.0);
        assert_eq!(t.action, SlAction::BelowMinR, "precondition");
        assert_eq!(
            pending_action(t, 0.0020, budget()),
            PendingAction::Demote,
            "pull it — don't let it fill at a ratio nobody accepted",
        );
    }

    /// The calm-again case end-to-end: a stop widened for a spike is given
    /// back once the spread calms, and the stake grows to match.
    #[test]
    fn the_widened_stop_is_given_back_when_the_spread_calms() {
        let t = sl_target(
            SpreadInputs::measured_only(0.00015),
            0.0020,
            0.0064,
            0.0200,
            1.0,
        );
        assert_eq!(t.action, SlAction::Shrink, "precondition");
        let PendingAction::Adjust { sl_distance, .. } = pending_action(t, 0.0064, budget()) else {
            panic!("expected a shrink back to the drawn stop");
        };
        assert!(
            (sl_distance - 0.0020).abs() < 1e-12,
            "back to the drawn stop, never past it",
        );
    }
}
