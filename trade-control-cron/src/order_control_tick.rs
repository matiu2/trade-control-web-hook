//! The live driver for the every-candle order-control re-check (rule 7).
//!
//! [`order_control_tick`] is what makes the `core::order_control` decisions
//! *happen*. Without it those modules are a complete, tested library that
//! nothing calls — and that was the state until this landed: a sub-1R entry
//! parked correctly (the enter path does that) and then **sat there until it was
//! dropped**, because nothing ever re-asked the question. The
//! `sgdjpy-spread-floor-min-r-block` fixture still booked 0R with three parked
//! orders in its log.
//!
//! Two jobs, per account, per tick:
//!
//! 1. **Promote** — re-ask each parked order whether it now clears its R-floor
//!    at the current spread, and place it when it does.
//! 2. **Re-price** — re-ask each *resting* order what stop it should carry, and
//!    cancel-and-replace it at the right stake when the answer has moved.
//!
//! Both questions are answered by the same pure
//! [`sl_target`](trade_control_core::order_control::sl_target), so a promotion
//! and a re-price cannot disagree about what a stop should be.
//!
//! # Why this belongs beside the other cron drivers
//!
//! Same shape as [`crate::spread_lifecycle`]: acquire the account's broker,
//! match the [`BrokerHandle`] **once** to a single `impl Broker` (the shared fns
//! are generic over `B: Broker`, which the enum cannot satisfy), build the live
//! [`SignedBodySource`], and call into `core`. Every decision lives in `core` so
//! replay and live share them
//! (`[[strategy_changes_in_both_replayer_and_worker]]`); this module owns only
//! the live glue.
//!
//! # Where the spread readings come from
//!
//! The `max` of three terms
//! ([`SpreadInputs`](trade_control_core::order_control::SpreadInputs)):
//!
//! - **measured** — one live `get_quote` per instrument, cached for the tick so
//!   N orders on one pair cost one round-trip.
//! - **expected this hour / next hour** — the baked forecast, via
//!   [`spread_forecast_frac`]. Free: a table lookup, no I/O.
//!
//! The forecast terms are what let a stop be sized for a spike *before* it
//! lands — the protection the 30-minute spread-hour lead gave as a step function
//! around flagged hours, now continuous and per-trade.
//!
//! # What it refuses to do
//!
//! An order whose geometry we cannot read is **left alone**, never guessed at.
//! That covers a row written before `OrderControlSnapshot` existed, a resting
//! order with no matching `EntryAttempt` (not ours), and a degenerate distance.
//! An order left resting at a slightly wrong size is recoverable next tick; one
//! cancelled on a guess is not.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use trade_control_core::broker::{Broker, PendingOrder};
use trade_control_core::dispatch_config::DispatchConfig;
use trade_control_core::incoming::Verified;
use trade_control_core::order_control::{
    PendingAction, RiskBudget, SlAction, SpreadInputs, pending_action, promote_stored_order,
    reprice_pending_order, sl_target,
};
use trade_control_core::pending_lifecycle::{
    EnterConfigProvider, SignedBodySource, VerifiedSource,
};
use trade_control_core::spread_blackout::spread_forecast_frac;
use trade_control_core::state::{EntryAttempt, StateStore};

use crate::broker_handle::BrokerHandle;
use crate::seam::CronEnv;

/// The live [`EnterConfigProvider`] — mirrors [`crate::spread_lifecycle`]'s:
/// forward to [`CronEnv::dispatch_config`] so a promoted or re-placed enter
/// sizes identically to a first-run one.
struct CronEnterConfigProvider<'c, C: CronEnv> {
    cron: &'c C,
}

impl<C: CronEnv> EnterConfigProvider for CronEnterConfigProvider<'_, C> {
    async fn dispatch_config(&self, verified: &Verified) -> DispatchConfig {
        self.cron.dispatch_config(verified).await
    }
}

/// Run the order-control re-check across every affected account.
pub async fn order_control_tick<S, C>(store: &S, cron: &C, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    for account in affected_accounts(store).await {
        tick_account(store, cron, account.as_deref(), now).await;
    }
}

/// The accounts with anything to re-check: those carrying tracked
/// `EntryAttempt` rows (resting orders to re-price) or held-trade records
/// (parked orders to promote).
async fn affected_accounts<S: StateStore>(store: &S) -> Vec<Option<String>> {
    let mut accounts: Vec<Option<String>> = Vec::new();
    let mut push = |acc: &Option<String>| {
        if !accounts.contains(acc) {
            accounts.push(acc.clone());
        }
    };
    match store.list_all_entry_attempts().await {
        Ok(v) => v.iter().for_each(|a| push(&a.account)),
        Err(err) => tracing::error!("order-control tick: list_all_entry_attempts: {err}"),
    }
    match store.list_all_held_trade_records().await {
        Ok(v) => v.iter().for_each(|r| push(&r.account)),
        Err(err) => tracing::error!("order-control tick: list records failed: {err}"),
    }
    accounts
}

/// One account: match the broker enum once, then run both halves.
async fn tick_account<S, C>(store: &S, cron: &C, account: Option<&str>, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    let scope = account.unwrap_or("<global>");
    // The signing key re-verifies a parked/stored body before it is re-driven.
    // Without it nothing here can be trusted, so skip the account rather than
    // act on an unverifiable payload — the same rail `spread_lifecycle` follows.
    let Some(key) = cron.signing_key() else {
        tracing::error!("order-control[{scope}]: no signing key; skipping account");
        return;
    };
    let Some(broker) = cron.acquire_broker(account).await else {
        tracing::error!("order-control[{scope}]: broker acquisition failed; skipping account");
        return;
    };
    let src = SignedBodySource { key: &key };
    let cfg = CronEnterConfigProvider { cron };

    match &broker {
        BrokerHandle::Oanda(b) => run_both(b, store, &cfg, &src, account, now).await,
        BrokerHandle::TradeNation(b) => run_both(b, store, &cfg, &src, account, now).await,
    }
}

/// Generic-over-broker body: promote parked orders, then re-price resting ones.
///
/// Promotion runs **first**. A promoted order is placed at the current spread,
/// so re-pricing it in the same tick would at best be redundant and at worst
/// cancel-and-replace an order placed seconds earlier.
async fn run_both<B, S, P, V>(
    broker: &B,
    store: &S,
    cfg: &P,
    src: &V,
    account: Option<&str>,
    now: DateTime<Utc>,
) where
    B: Broker,
    S: StateStore,
    P: EnterConfigProvider,
    V: VerifiedSource,
{
    let mut quotes = QuoteCache::default();
    promote_pass(broker, store, cfg, src, account, &mut quotes, now).await;
    reprice_pass(broker, store, cfg, src, account, &mut quotes, now).await;
}

/// One live spread reading per instrument per tick.
///
/// N resting orders on one pair would otherwise cost N quote round-trips on a
/// loop that runs every few seconds. A failed quote is cached as `None` too, so
/// a broker having a bad minute isn't retried once per order.
#[derive(Default)]
struct QuoteCache(HashMap<String, Option<f64>>);

impl QuoteCache {
    /// The measured spread for `instrument`, or `None` if unavailable.
    async fn spread<B: Broker>(&mut self, broker: &B, instrument: &str) -> Option<f64> {
        if let Some(hit) = self.0.get(instrument) {
            return *hit;
        }
        let measured = match broker.get_quote(instrument).await {
            Ok(q) => Some(q.spread()),
            Err(err) => {
                tracing::warn!("order-control: get_quote({instrument}) failed: {err:?}");
                None
            }
        };
        self.0.insert(instrument.to_string(), measured);
        measured
    }
}

/// Assemble the three spread terms for an instrument at `now`.
///
/// A quote we could not read contributes `0.0` rather than blocking the whole
/// decision: the baked forecast still applies, and `sl_target` drops degenerate
/// readings from the `max`. Failing the other way — refusing to act without a
/// quote — would leave stops un-widened exactly when the broker is struggling.
async fn spread_inputs<B: Broker>(
    broker: &B,
    quotes: &mut QuoteCache,
    instrument: &str,
    now: DateTime<Utc>,
) -> SpreadInputs {
    let measured = quotes.spread(broker, instrument).await.unwrap_or(0.0);
    let (expected_this_hour, expected_next_hour) = spread_forecast_frac(instrument, now);
    SpreadInputs {
        last_candle: measured,
        expected_this_hour,
        expected_next_hour,
    }
}

// --- promote: Stored → Pending ------------------------------------------------

/// Re-ask every parked order whether it now clears its R-floor.
async fn promote_pass<B, S, P, V>(
    broker: &B,
    store: &S,
    cfg: &P,
    src: &V,
    account: Option<&str>,
    quotes: &mut QuoteCache,
    now: DateTime<Utc>,
) where
    B: Broker,
    S: StateStore,
    P: EnterConfigProvider,
    V: VerifiedSource,
{
    let records = match store.list_all_held_trade_records().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("order-control promote: list records: {err}");
            return;
        }
    };
    for record in records
        .iter()
        .filter(|r| r.account.as_deref() == account && !r.stored_orders.is_empty())
    {
        let Some(parked) = record.stored_orders.first() else {
            continue;
        };
        let spreads = spread_inputs(broker, quotes, &record.instrument, now).await;
        // A parked order has no stop distinct from its drawn one — it was never
        // placed — so the drawn distance is both the original and the current.
        let target = sl_target(
            spreads,
            parked.original_sl_distance,
            parked.original_sl_distance,
            parked.tp_distance,
            parked.min_r,
        );
        let clears_min_r = target.action != SlAction::BelowMinR;
        match promote_stored_order(broker, store, cfg, src, &record.trade_id, clears_min_r, now)
            .await
        {
            Ok(outcome) => tracing::debug!(
                "order-control promote[{}]: {outcome:?} (r={:.3}, clears={clears_min_r})",
                record.trade_id,
                target.r,
            ),
            Err(err) => tracing::error!("order-control promote[{}]: {err}", record.trade_id),
        }
    }
}

// --- re-price: an already-resting order ---------------------------------------

/// Re-ask every resting order what stop it should carry, and act when it moved.
async fn reprice_pass<B, S, P, V>(
    broker: &B,
    store: &S,
    cfg: &P,
    src: &V,
    account: Option<&str>,
    quotes: &mut QuoteCache,
    now: DateTime<Utc>,
) where
    B: Broker,
    S: StateStore,
    P: EnterConfigProvider,
    V: VerifiedSource,
{
    let resting = match broker.list_pending_orders(account.unwrap_or("")).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("order-control reprice: list_pending_orders: {err:?}");
            return;
        }
    };
    if resting.is_empty() {
        return;
    }
    let attempts = match store.list_all_entry_attempts().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("order-control reprice: list_all_entry_attempts: {err}");
            return;
        }
    };
    for order in &resting {
        reprice_one(
            broker, store, cfg, src, order, &attempts, account, quotes, now,
        )
        .await;
    }
}

/// One resting order: decide, then act.
#[allow(clippy::too_many_arguments)]
async fn reprice_one<B, S, P, V>(
    broker: &B,
    store: &S,
    cfg: &P,
    src: &V,
    order: &PendingOrder,
    attempts: &[EntryAttempt],
    account: Option<&str>,
    quotes: &mut QuoteCache,
    now: DateTime<Utc>,
) where
    B: Broker,
    S: StateStore,
    P: EnterConfigProvider,
    V: VerifiedSource,
{
    // Matched on the broker's own order id — the exact join, never the coarse
    // `(instrument, direction, account)` fallback. An order we can't identify
    // exactly is not ours to cancel: the aliasing that fallback tolerates for a
    // *stop amend* would here cancel-and-replace the wrong trade's order.
    let Some(attempt) = attempts
        .iter()
        .find(|a| a.broker_order_id == order.order_id)
    else {
        return;
    };
    let Some(geometry) = geometry_of(attempt, order) else {
        return;
    };

    let spreads = spread_inputs(broker, quotes, &order.instrument, now).await;
    let target = sl_target(
        spreads,
        geometry.original_sl_distance,
        geometry.current_sl_distance,
        geometry.tp_distance,
        geometry.min_r,
    );
    let action = pending_action(target, geometry.current_sl_distance, geometry.budget);
    if action == PendingAction::Hold {
        return;
    }

    match reprice_pending_order(
        broker,
        store,
        cfg,
        src,
        order,
        account,
        action,
        attempt.expires_at,
        geometry.bar_seconds,
        now,
    )
    .await
    {
        Ok(outcome) => tracing::info!(
            "order-control reprice[{}]: {outcome:?} (r={:.3})",
            attempt.trade_id,
            target.r,
        ),
        Err(err) => tracing::error!("order-control reprice[{}]: {err}", attempt.trade_id),
    }
}

/// The geometry one resting order is judged against, in the price-unit
/// distances `sl_target` expects.
struct Geometry {
    original_sl_distance: f64,
    current_sl_distance: f64,
    tp_distance: f64,
    min_r: f64,
    budget: RiskBudget,
    bar_seconds: i64,
}

/// Read a resting order's geometry off its `EntryAttempt`, or `None` when the
/// row can't support a decision.
///
/// `None` throughout means "leave it alone": a row with no
/// [`OrderControlSnapshot`] predates the field or came from a path with no
/// intent (admin `adopt-trade`), and a degenerate distance is unjudgeable.
/// Neither is a reason to cancel a live order.
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
        // preserves the risk the entry path originally sized, and this module
        // never needs an account balance — which also means it cannot drift from
        // that sizing as the balance moves.
        budget: RiskBudget::absolute(order.stake * current_sl_distance),
        bar_seconds: snapshot.bar_seconds.unwrap_or(3600),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_core::intent::Direction;
    use trade_control_core::state::OrderControlSnapshot;

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
            cancel_at: None,
            pip_size: Some(0.0001),
            blackout_close: Default::default(),
            breakeven: None,
            order_control: snapshot,
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
    /// `reprice_one` uses.
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
}
