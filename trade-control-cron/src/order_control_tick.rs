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
use trade_control_core::broker::Broker;
use trade_control_core::dispatch_config::DispatchConfig;
use trade_control_core::incoming::Verified;
use trade_control_core::order_control::{
    PromoteScope, SpreadSource, promote_due_orders, reprice_due_orders,
};
use trade_control_core::pending_lifecycle::{
    EnterConfigProvider, SignedBodySource, VerifiedSource,
};
use trade_control_core::state::StateStore;

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
        BrokerHandle::Ibkr(b) => run_both(b, store, &cfg, &src, account, now).await,
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
    let mut quotes = QuoteCache::new(broker);
    // Scope this pass to the account whose broker we just acquired — a `None`
    // fan-out entry means the worker-global rows, NOT every account's.
    let scope = account.map_or(PromoteScope::Global, PromoteScope::Account);
    promote_due_orders(broker, store, cfg, src, scope, now).await;
    reprice_due_orders(broker, store, cfg, src, &mut quotes, account, now).await;
}

/// One live spread reading per instrument per tick, as a
/// [`SpreadSource`](trade_control_core::order_control::SpreadSource).
///
/// N resting orders on one pair would otherwise cost N quote round-trips on a
/// loop that runs every few seconds. A failed quote is cached as `None` too, so
/// a broker having a bad minute isn't retried once per order.
///
/// This caching is the ONLY thing the live re-price pass does differently from
/// the offline one, which is exactly why the spread reading is a trait: the
/// replay reads its recorded book locally, where a cache would buy nothing.
struct QuoteCache<'b, B: Broker> {
    broker: &'b B,
    seen: HashMap<String, Option<f64>>,
}

impl<'b, B: Broker> QuoteCache<'b, B> {
    fn new(broker: &'b B) -> Self {
        Self {
            broker,
            seen: HashMap::new(),
        }
    }
}

impl<B: Broker> SpreadSource for QuoteCache<'_, B> {
    async fn measured(&mut self, instrument: &str) -> Option<f64> {
        if let Some(hit) = self.seen.get(instrument) {
            return *hit;
        }
        let measured = match self.broker.get_quote(instrument).await {
            Ok(q) => Some(q.spread()),
            Err(err) => {
                tracing::warn!("order-control: get_quote({instrument}) failed: {err:?}");
                None
            }
        };
        self.seen.insert(instrument.to_string(), measured);
        measured
    }
}

// --- promote / re-price -------------------------------------------------------
//
// Both decisions live in `core::order_control` so the offline replay runs the
// SAME passes — see `core::order_control::tick` and
// `core::order_control::reprice_pass`. Nothing to do here but hand each the
// account's broker and, for the re-price, the per-tick quote cache above.
