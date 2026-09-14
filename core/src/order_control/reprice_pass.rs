//! The **re-price** half of the every-candle order-control tick (rule 7),
//! shared by the live cron and the offline replay.
//!
//! [`promote_due_orders`](super::promote_due_orders) re-asks a *parked* order
//! whether it may go on. This module asks the neighbouring question of an order
//! that is already **resting**: is the stop it carries still the right one at
//! today's spread, and if not, what should be done about it?
//!
//! # Why this had to move out of the cron driver
//!
//! It was born in `trade-control-cron::order_control_tick::reprice_pass`, whose
//! sibling [`tick`](super::tick) module docs asserted the re-price half "needs a
//! live `list_pending_orders` join against `EntryAttempt` rows and a real
//! cancel-and-replace at a broker — machinery the replay models differently".
//! **That is no longer true**, and leaving it true-by-assertion cost the fixture
//! corpus its only view of the forward-looking SL floor:
//!
//! - The replay broker *does* report resting orders from its held ledger
//!   (`list_pending_orders`, S7) — that is what the shared
//!   [`pending_order_lifecycle`](crate::pending_lifecycle) already lists to
//!   cancel through a spread hour.
//! - The replay *does* write `EntryAttempt` rows carrying an
//!   [`OrderControlSnapshot`](crate::state::OrderControlSnapshot), because it
//!   drives the same `run_enter` that records them.
//! - The replay *does* model cancel-and-replace-at-a-new-stop: `cancel_order`
//!   flags the held order, and `place_entry` with no armed placement
//!   re-activates it and **refreshes its stored levels from the fresh request**.
//!
//! So the only thing keeping this pass live-only was the doc comment. Slice 7
//! ([`crate::pending_lifecycle`]) deliberately retired the `HoldReason::SpreadHour`
//! ON-side derivation in favour of the forward-looking SL floor, which is
//! delivered *exclusively* here. Offline the replay therefore had **neither** the
//! retired hold **nor** its replacement: a resting order sat at its original stop
//! through a forecast spread spike while live re-priced or parked it.
//!
//! # What this module owns, and what it does not
//!
//! It owns the **join and the loop**: resting order → its `EntryAttempt` → the
//! geometry → [`sl_target`] → [`pending_action`] → [`reprice_pending_order`].
//! Every one of those is already shared; what was not shared was the wiring
//! between them, which is exactly where the two sides had drifted.
//!
//! It does **not** own the spread reading. The caller supplies a
//! [`SpreadSource`], because the live cron caches one `get_quote` per instrument
//! per tick across N orders while the replay answers from its recorded book.
//!
//! # What it refuses to do
//!
//! An order whose geometry cannot be read is **left alone**, never guessed at:
//! a row written before `OrderControlSnapshot` existed, a resting order with no
//! matching `EntryAttempt` (not ours), or a degenerate distance. An order left
//! resting at a slightly wrong size is recoverable next tick; one cancelled on a
//! guess is not.

use chrono::{DateTime, Utc};

use super::pending::{PendingAction, RiskBudget, pending_action};
use super::reprice::{ParkGeometry, RepriceOutcome, reprice_pending_order};
use super::sl_target::{SpreadInputs, sl_target};
use crate::broker::{Broker, PendingOrder};
use crate::pending_lifecycle::{EnterConfigProvider, VerifiedSource};
use crate::spread_blackout::spread_forecast_frac;
use crate::state::{EntryAttempt, StateStore};

/// Where the *measured* spread term comes from.
///
/// A trait rather than a plain `f64` argument because the two callers read it
/// very differently and neither reading belongs to this module:
///
/// - **Live** — one `get_quote` round-trip per instrument, cached for the tick
///   so N resting orders on one pair cost one call.
/// - **Replay** — the recorded bid/ask book at the bar being replayed.
///
/// A source that cannot answer returns `None`, which contributes `0.0` to the
/// `max` rather than blocking the decision: the baked forecast still applies,
/// and [`sl_target`] drops degenerate readings. Failing the other way — refusing
/// to act without a quote — would leave stops un-widened exactly when the broker
/// is struggling.
pub trait SpreadSource {
    /// The measured spread for `instrument` in price units, or `None`.
    fn measured(&mut self, instrument: &str) -> impl Future<Output = Option<f64>>;
}

/// A [`SpreadSource`] that asks the broker directly, with no caching.
///
/// What the replay uses: its `get_quote` is a local read off the bar it is
/// already holding, so a cache would buy nothing and could only go stale within
/// a bar.
pub struct BrokerQuotes<'b, B: Broker>(pub &'b B);

impl<B: Broker> SpreadSource for BrokerQuotes<'_, B> {
    async fn measured(&mut self, instrument: &str) -> Option<f64> {
        match self.0.get_quote(instrument).await {
            Ok(q) => Some(q.spread()),
            Err(err) => {
                tracing::warn!("order-control reprice: get_quote({instrument}) failed: {err:?}");
                None
            }
        }
    }
}

/// The geometry one resting order is judged against, in the price-unit
/// distances [`sl_target`] expects.
struct Geometry {
    original_sl_distance: f64,
    current_sl_distance: f64,
    tp_distance: f64,
    min_r: f64,
    budget: RiskBudget,
    bar_seconds: i64,
}

/// Read a resting order's geometry off its [`EntryAttempt`], or `None` when the
/// row cannot support a decision.
///
/// `None` throughout means "leave it alone": a row with no
/// [`OrderControlSnapshot`](crate::state::OrderControlSnapshot) predates the
/// field or came from a path with no intent (admin `adopt-trade`), and a
/// degenerate distance is unjudgeable. Neither is a reason to cancel a live
/// order.
fn geometry_of(attempt: &EntryAttempt, order: &PendingOrder) -> Option<Geometry> {
    let snapshot = attempt.order_control.as_ref()?;
    let placed_stop = attempt.stop_loss_price?;
    // Distances are measured from the order's own trigger — the price it will
    // fill at — not from a stale reference price on the row.
    let current_sl_distance = (order.trigger - placed_stop).abs();
    let original_sl_distance = (order.trigger - snapshot.original_stop_loss).abs();
    let tp_distance = (snapshot.take_profit_price - order.trigger).abs();
    if !(current_sl_distance.is_finite() && current_sl_distance > 0.0) {
        return None;
    }
    if !(order.stake.is_finite() && order.stake > 0.0) {
        return None;
    }
    Some(Geometry {
        original_sl_distance,
        current_sl_distance,
        tp_distance,
        min_r: snapshot.min_r,
        // Risk is reconstructed from the order AS PLACED: whatever it currently
        // stakes over whatever it currently risks *is* the budget. So a re-size
        // preserves the risk the entry path originally sized, and this pass
        // never needs an account balance — which also means it cannot drift from
        // that sizing as the balance moves.
        budget: RiskBudget::absolute(order.stake * current_sl_distance),
        bar_seconds: snapshot.bar_seconds.unwrap_or(3600),
    })
}

/// Assemble the three spread terms for `instrument` at `now`.
async fn spread_inputs<Q: SpreadSource>(
    quotes: &mut Q,
    instrument: &str,
    now: DateTime<Utc>,
) -> SpreadInputs {
    let measured = quotes.measured(instrument).await.unwrap_or(0.0);
    let (expected_this_hour, expected_next_hour) = spread_forecast_frac(instrument, now);
    SpreadInputs {
        last_candle: measured,
        expected_this_hour,
        expected_next_hour,
    }
}

/// Re-ask every resting order what stop it should carry, and act when it moved.
///
/// Returns one `(trade_id, outcome)` per order that was actually acted on — a
/// `Hold` is not reported, because the overwhelmingly common answer is "nothing
/// happened" and a caller that logged every one would drown.
///
/// Per-order failures are logged and skipped: one unjudgeable resting order must
/// never stop the rest of the pass, exactly as [`promote_due_orders`] refuses to
/// abort on one bad record.
///
/// [`promote_due_orders`]: super::promote_due_orders
pub async fn reprice_due_orders<B, S, P, V, Q>(
    broker: &B,
    store: &S,
    cfg: &P,
    src: &V,
    quotes: &mut Q,
    account: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<(String, RepriceOutcome)>
where
    B: Broker,
    S: StateStore,
    P: EnterConfigProvider,
    V: VerifiedSource,
    Q: SpreadSource,
{
    let resting = match broker.list_pending_orders(account.unwrap_or("")).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("order-control reprice: list_pending_orders: {err:?}");
            return Vec::new();
        }
    };
    if resting.is_empty() {
        return Vec::new();
    }
    let attempts = match store.list_all_entry_attempts().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("order-control reprice: list_all_entry_attempts: {err}");
            return Vec::new();
        }
    };
    let mut outcomes = Vec::new();
    for order in &resting {
        if let Some(out) = reprice_one(
            broker, store, cfg, src, order, &attempts, account, quotes, now,
        )
        .await
        {
            outcomes.push(out);
        }
    }
    outcomes
}

/// One resting order: decide, then act. `None` when nothing was done.
#[allow(clippy::too_many_arguments)]
async fn reprice_one<B, S, P, V, Q>(
    broker: &B,
    store: &S,
    cfg: &P,
    src: &V,
    order: &PendingOrder,
    attempts: &[EntryAttempt],
    account: Option<&str>,
    quotes: &mut Q,
    now: DateTime<Utc>,
) -> Option<(String, RepriceOutcome)>
where
    B: Broker,
    S: StateStore,
    P: EnterConfigProvider,
    V: VerifiedSource,
    Q: SpreadSource,
{
    // Matched on the broker's own order id — the exact join, never the coarse
    // `(instrument, direction, account)` fallback. An order we can't identify
    // exactly is not ours to cancel: the aliasing that fallback tolerates for a
    // *stop amend* would here cancel-and-replace the wrong trade's order.
    let attempt = attempts
        .iter()
        .find(|a| a.broker_order_id == order.order_id)?;
    let geometry = geometry_of(attempt, order)?;

    let spreads = spread_inputs(quotes, &order.instrument, now).await;
    let target = sl_target(
        spreads,
        geometry.original_sl_distance,
        geometry.current_sl_distance,
        geometry.tp_distance,
        geometry.min_r,
    );
    let action = pending_action(target, geometry.current_sl_distance, geometry.budget);
    if action == PendingAction::Hold {
        return None;
    }

    match reprice_pending_order(
        broker,
        store,
        cfg,
        src,
        order,
        account,
        action,
        // The DRAWN geometry, so a park this demote writes can be judged by the
        // promotion gate against the same question — not promoted blind straight
        // back into the demote.
        ParkGeometry {
            original_sl_distance: geometry.original_sl_distance,
            tp_distance: geometry.tp_distance,
            min_r: geometry.min_r,
        },
        attempt.expires_at,
        geometry.bar_seconds,
        now,
    )
    .await
    {
        Ok(outcome) => {
            tracing::info!(
                "order-control reprice[{}]: {outcome:?} (r={:.3})",
                attempt.trade_id,
                target.r,
            );
            Some((attempt.trade_id.clone(), outcome))
        }
        Err(err) => {
            tracing::error!("order-control reprice[{}]: {err}", attempt.trade_id);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::Direction;
    use crate::state::{MemStateStore, OrderControlSnapshot};
    use std::cell::RefCell;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    fn order(trigger: f64, stake: f64) -> PendingOrder {
        PendingOrder {
            order_id: "ord-1".into(),
            instrument: "EUR_USD".into(),
            direction: Direction::Long,
            trigger,
            is_stop: true,
            stake,
        }
    }

    fn attempt(stop: Option<f64>, snapshot: Option<OrderControlSnapshot>) -> EntryAttempt {
        EntryAttempt {
            trade_id: "t-1".into(),
            account: None,
            instrument: "EUR_USD".into(),
            attempt_no: 1,
            broker_order_id: "ord-1".into(),
            broker_trade_id: None,
            direction: Direction::Long,
            placed_at: ts("2026-07-22T12:00:00Z"),
            shell_time: ts("2026-07-22T12:00:00Z"),
            expires_at: ts("2026-07-24T00:00:00Z"),
            stop_loss_price: stop,
            adverse_extreme: None,
            cancel_at: None,
            pip_size: Some(0.0001),
            blackout_close: Default::default(),
            breakeven: None,
            order_control: snapshot,
            superseded: false,
        }
    }

    fn snapshot() -> OrderControlSnapshot {
        OrderControlSnapshot {
            // Drawn 20 pips below the 1.1000 trigger; the row's placed stop is
            // wider (already floor-widened), which is the interesting case.
            original_stop_loss: 1.0980,
            take_profit_price: 1.1200,
            min_r: 1.0,
            bar_seconds: Some(3600),
        }
    }

    /// The geometry read is the whole contract with `sl_target`: distances are
    /// measured from the ORDER'S TRIGGER, and the drawn stop stays distinct from
    /// the placed one.
    #[test]
    fn geometry_measures_distances_from_the_orders_trigger() {
        let g = geometry_of(
            &attempt(Some(1.0950), Some(snapshot())),
            &order(1.1000, 10_000.0),
        )
        .expect("full geometry");
        assert!(
            (g.current_sl_distance - 0.0050).abs() < 1e-12,
            "placed stop 1.0950 is 50 pips from the 1.1000 trigger, got {}",
            g.current_sl_distance,
        );
        assert!(
            (g.original_sl_distance - 0.0020).abs() < 1e-12,
            "drawn stop 1.0980 is 20 pips from the trigger, got {}",
            g.original_sl_distance,
        );
        assert!((g.tp_distance - 0.0200).abs() < 1e-12);
        assert!((g.min_r - 1.0).abs() < 1e-12);
    }

    /// Risk is reconstructed from the order as placed, so a re-size preserves
    /// exactly what the entry path sized — no account balance needed.
    ///
    /// Mutation check: divide instead of multiply and this goes red.
    #[test]
    fn the_budget_is_the_risk_the_order_actually_carries() {
        let g = geometry_of(
            &attempt(Some(1.0950), Some(snapshot())),
            &order(1.1000, 10_000.0),
        )
        .expect("full geometry");
        // 10,000 units over a 0.0050 stop = $50 at risk.
        assert!(
            (g.budget.amount - 50.0).abs() < 1e-9,
            "expected $50 at risk, got {}",
            g.budget.amount,
        );
    }

    /// A row written before `OrderControlSnapshot` existed is LEFT ALONE. This
    /// is the deploy-boundary case: guessing here would cancel-and-replace live
    /// orders on geometry we don't have.
    ///
    /// Mutation check: substitute a default snapshot and this goes red.
    #[test]
    fn a_legacy_row_without_the_snapshot_is_left_alone() {
        assert!(
            geometry_of(&attempt(Some(1.0950), None), &order(1.1000, 10_000.0)).is_none(),
            "no snapshot ⇒ no decision",
        );
    }

    /// Degenerate inputs are unjudgeable, never a reason to touch a live order.
    #[test]
    fn degenerate_rows_are_unjudgeable() {
        // No placed stop at all.
        assert!(geometry_of(&attempt(None, Some(snapshot())), &order(1.1000, 10_000.0)).is_none());
        // Stop sits exactly on the trigger — a zero distance can't be sized.
        assert!(
            geometry_of(
                &attempt(Some(1.1000), Some(snapshot())),
                &order(1.1000, 10_000.0)
            )
            .is_none(),
            "a zero stop distance must not resolve to an infinite stake",
        );
        // A stake of zero would make the reconstructed budget zero.
        assert!(
            geometry_of(
                &attempt(Some(1.0950), Some(snapshot())),
                &order(1.1000, 0.0)
            )
            .is_none(),
        );
    }

    /// End-to-end through the REAL decision functions: a spike forecast for the
    /// coming hour re-sizes a resting order before the spike lands. This is the
    /// behaviour the whole slice exists for, driven through the same call chain
    /// [`reprice_one`] uses.
    #[test]
    fn a_forecast_spike_resizes_a_resting_order() {
        let g = geometry_of(
            &attempt(Some(1.0980), Some(snapshot())),
            &order(1.1000, 50_000.0),
        )
        .expect("full geometry");
        // Calm measured spread, but the next hour forecasts a real EUR/USD spike.
        let spreads = SpreadInputs {
            last_candle: 0.00015,
            expected_this_hour: 0.00015,
            expected_next_hour: 0.00064,
        };
        let target = sl_target(
            spreads,
            g.original_sl_distance,
            g.current_sl_distance,
            g.tp_distance,
            g.min_r,
        );
        let PendingAction::Adjust { sl_distance, stake } =
            pending_action(target, g.current_sl_distance, g.budget)
        else {
            panic!("the forecast must widen this resting order");
        };
        assert!(
            (sl_distance - 0.0064).abs() < 1e-12,
            "sized off the FORECAST, not the calm measurement",
        );
        assert!(
            stake < 50_000.0,
            "a wider stop must take a smaller stake, got {stake}",
        );
        assert!(
            (stake * sl_distance - g.budget.amount).abs() < 1e-6,
            "and the risk carried must not move",
        );
    }

    // ---- the PASS itself (the join + the loop), not just the geometry -------

    /// TWO resting orders on the SAME instrument, whose `EntryAttempt` rows carry
    /// deliberately different geometry. Only the second one is below its R-floor,
    /// so only the second may be demoted.
    ///
    /// This is what pins the **exact order-id join**. A pass that matched on
    /// `instrument` instead — the coarse `(instrument, direction, account)`
    /// fallback the module docs forbid — reads the FIRST row's geometry for both
    /// orders, and then either demotes both or neither. A single-order test cannot
    /// see that at all: the coarse join survives it, which is exactly how it
    /// survived this test set's first draft.
    #[test]
    fn the_join_is_on_the_order_id_so_two_orders_get_their_own_geometry() {
        let store = MemStateStore::default();
        // ord-A: TP 200 pips over a 50-pip stop ⇒ comfortably above min_r.
        pollster::block_on(store.record_entry_attempt(pass_attempt("A", 1.0950, 1.1200)))
            .expect("record A");
        // ord-B: TP only 20 pips over the same trigger ⇒ below its 1.0 R-floor
        // once the spread floor widens the stop, so it must be DEMOTED.
        pollster::block_on(store.record_entry_attempt(pass_attempt("B", 1.0950, 1.1020)))
            .expect("record B");

        let broker = PassBroker {
            orders: vec![pass_order("A"), pass_order("B")],
            cancels: RefCell::new(Vec::new()),
        };
        let outcomes = pollster::block_on(reprice_due_orders(
            &broker,
            &store,
            &TestCfg,
            &TestSrc::Ok,
            &mut BrokerQuotes(&broker),
            None,
            at("2026-07-22T13:30:00Z"),
        ));

        let demoted: Vec<&str> = outcomes
            .iter()
            .filter(|(_, o)| *o == RepriceOutcome::Demoted)
            .map(|(t, _)| t.as_str())
            .collect();
        assert_eq!(
            demoted,
            ["t-B"],
            "only the order whose OWN row is below min_r may be demoted — a coarse \
             instrument join reads one row for both, got {outcomes:?}",
        );
        assert_eq!(
            broker.cancels.borrow().as_slice(),
            ["ord-B"],
            "and only that order comes off the broker",
        );
    }

    /// An order with no matching `EntryAttempt` at all is **not ours** and must be
    /// left strictly alone — never cancelled on a guess. Pinned separately because
    /// the coarse join turns this case into a false match too.
    #[test]
    fn a_resting_order_with_no_attempt_row_is_never_touched() {
        let store = MemStateStore::default();
        pollster::block_on(store.record_entry_attempt(pass_attempt("A", 1.0950, 1.1020)))
            .expect("record A");
        let broker = PassBroker {
            // Only the UNKNOWN order rests; A's row exists but A does not.
            orders: vec![pass_order("Z")],
            cancels: RefCell::new(Vec::new()),
        };
        let outcomes = pollster::block_on(reprice_due_orders(
            &broker,
            &store,
            &TestCfg,
            &TestSrc::Ok,
            &mut BrokerQuotes(&broker),
            None,
            at("2026-07-22T13:30:00Z"),
        ));
        assert!(outcomes.is_empty(), "nothing to decide, got {outcomes:?}");
        assert!(
            broker.cancels.borrow().is_empty(),
            "an unidentifiable order must never be cancelled",
        );
    }

    /// A `Hold` — the overwhelmingly common answer — must produce **no outcome
    /// and no broker traffic**. Both matter: the caller logs every returned
    /// outcome, so a pass that reported holds would drown a ~5s cron in noise,
    /// and a `Hold` that reached the broker would open the unguarded
    /// cancel-and-replace gap for an order that did not need to move.
    ///
    /// The order rests at exactly the 10x floor (a 0.0050 stop against the 5-pip
    /// quote), so there is nothing to do.
    ///
    /// Mutation check: drop the `PendingAction::Hold` early return in
    /// `reprice_one` and this goes red on the `outcomes` assertion. That guard is
    /// duplicated inside `reprice_pending_order`, so WITHOUT this test the
    /// removal is invisible — the inner guard silently covers for it and the
    /// pass's own contract goes unpinned.
    #[test]
    fn a_hold_reports_nothing_and_reaches_no_broker() {
        let store = MemStateStore::default();
        pollster::block_on(store.record_entry_attempt(pass_attempt("A", 1.0950, 1.1200)))
            .expect("record A");
        let broker = PassBroker {
            orders: vec![pass_order("A")],
            cancels: RefCell::new(Vec::new()),
        };
        let outcomes = pollster::block_on(reprice_due_orders(
            &broker,
            &store,
            &TestCfg,
            &TestSrc::Ok,
            &mut BrokerQuotes(&broker),
            None,
            at("2026-07-22T13:30:00Z"),
        ));
        assert!(
            outcomes.is_empty(),
            "a hold is not an outcome worth reporting, got {outcomes:?}",
        );
        assert!(
            broker.cancels.borrow().is_empty(),
            "a hold must never reach the broker",
        );
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    fn pass_order(id: &str) -> PendingOrder {
        PendingOrder {
            order_id: format!("ord-{id}"),
            instrument: "EUR_USD".into(),
            direction: Direction::Long,
            trigger: 1.1000,
            is_stop: true,
            stake: 10_000.0,
        }
    }

    fn pass_attempt(id: &str, stop: f64, take_profit: f64) -> EntryAttempt {
        let mut a = attempt(
            Some(stop),
            Some(OrderControlSnapshot {
                original_stop_loss: 1.0980,
                take_profit_price: take_profit,
                min_r: 1.0,
                bar_seconds: Some(3600),
            }),
        );
        a.trade_id = format!("t-{id}");
        a.broker_order_id = format!("ord-{id}");
        a
    }

    /// A broker that rests exactly the orders it is given and records cancels.
    ///
    /// `get_quote` reports a 5-pip spread, so the shared 10x floor is 0.0050.
    /// That is deliberately calibrated to land BETWEEN the two rows: order A's
    /// 200-pip TP clears its 1.0 R-floor over it (R = 4.0), order B's 20-pip TP
    /// does not (R = 0.4). So a correct pass demotes exactly one of them, and any
    /// implementation that reads one row for both demotes either none or both.
    struct PassBroker {
        orders: Vec<PendingOrder>,
        cancels: RefCell<Vec<String>>,
    }

    impl Broker for PassBroker {
        async fn list_pending_orders(
            &self,
            _account_id: &str,
        ) -> Result<Vec<PendingOrder>, crate::broker::LookupError> {
            Ok(self.orders.clone())
        }
        async fn get_quote(
            &self,
            _instrument: &str,
        ) -> Result<crate::broker::Quote, crate::broker::LookupError> {
            Ok(crate::broker::Quote {
                bid: 1.09975,
                ask: 1.10025,
            })
        }
        async fn cancel_order(
            &self,
            _account_id: &str,
            broker_order_id: &str,
        ) -> Result<(), crate::broker::CancelError> {
            self.cancels.borrow_mut().push(broker_order_id.to_string());
            Ok(())
        }
        async fn place_entry(
            &self,
            _max_risk_pct: f64,
            _max_open_positions: u32,
            _req: &crate::broker::EntryRequest<'_>,
        ) -> Result<crate::broker::Placement, crate::broker::EntryError> {
            Ok(crate::broker::Placement::id_only("ord-new"))
        }
        async fn close_positions(&self, _instrument: &str) -> crate::broker::CloseOutcome {
            crate::broker::CloseOutcome::NothingOpen
        }
        async fn cancel_pending_for_instrument(&self, _instrument: &str) -> usize {
            0
        }
        async fn lookup_attempt_state(
            &self,
            _instrument: &str,
            _broker_order_id: &str,
            _broker_trade_id: Option<&str>,
        ) -> Result<crate::broker::AttemptState, crate::broker::LookupError> {
            Ok(crate::broker::AttemptState::Unknown)
        }
        async fn list_open_positions(
            &self,
            _account_id: &str,
        ) -> Result<Vec<crate::broker::OpenPosition>, crate::broker::LookupError> {
            Ok(vec![])
        }
        async fn amend_stop(
            &self,
            _account_id: &str,
            _position_or_order_id: &str,
            _new_stop: f64,
        ) -> Result<(), crate::broker::AmendError> {
            Ok(())
        }
        async fn get_candles(
            &self,
            _instrument: &str,
            _granularity: crate::broker::Granularity,
            _since: DateTime<Utc>,
            _now: DateTime<Utc>,
        ) -> Result<Vec<crate::broker::Candle>, crate::broker::CandleError> {
            Ok(vec![])
        }
    }

    struct TestCfg;
    impl EnterConfigProvider for TestCfg {
        async fn dispatch_config(
            &self,
            _verified: &crate::incoming::Verified,
        ) -> crate::dispatch_config::DispatchConfig {
            crate::dispatch_config::DispatchConfig {
                worker_max_risk_pct: 1.0,
                worker_max_open_positions: 3,
                pip_size: 0.0001,
                tick_size: None,
                caps: Default::default(),
            }
        }
    }

    enum TestSrc {
        Ok,
    }

    impl VerifiedSource for TestSrc {
        async fn recover(
            &self,
            key: &str,
            _signed_body: Option<&str>,
            _now: DateTime<Utc>,
        ) -> crate::pending_lifecycle::Recovered {
            let trade = key.strip_prefix("ord-").unwrap_or("A");
            let intent: crate::intent::Intent = serde_json::from_str(&format!(
                r#"{{
                    "v": 1, "id": "t-{trade}-enter",
                    "not_after": "2026-07-24T00:00:00Z",
                    "action": "enter", "instrument": "EUR_USD", "direction": "long",
                    "entry": {{ "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.1000 }},
                    "stop_loss": {{ "absolute": 1.0950 }},
                    "take_profit": {{ "absolute": 1.1200 }},
                    "broker": "oanda", "trade_id": "t-{trade}", "pip_size": 0.0001
                }}"#
            ))
            .expect("valid enter intent");
            let shell = crate::intent::Shell::from_candle(&crate::broker::Candle {
                time: at("2026-07-22T12:00:00Z"),
                o: 1.0990,
                h: 1.1005,
                l: 1.0985,
                c: 1.0995,
            });
            crate::pending_lifecycle::Recovered::Ok(Box::new(crate::incoming::Verified {
                shell,
                intent,
            }))
        }
    }
}
