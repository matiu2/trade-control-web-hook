//! SL-breach + expiry sweep of pending `EntryAttempt` rows.
//!
//! Runs on a cron schedule. For each tracked `EntryAttempt` either:
//!
//! * its `expires_at` has passed → cancel + delete (the alert window
//!   itself is dead, so the still-pending order should be too); or
//! * its `cancel_at` (bar-based expiry) has passed → cancel + delete; or
//! * price has **traded past** its `stop_loss_price` at any point since
//!   placement → the setup is invalidated before it ever filled, cancel +
//!   delete. Note "since placement", not "right now": the row carries a running
//!   adverse extreme so the decision is a property of the price path rather than
//!   of where the cron tick happened to land on it. See [`maybe_breach_cancel`].
//!
//! Each of those is **terminal**: the setup is dead and the row goes with it.
//!
//! # Market hours are a hold, not a sweep reason (v123)
//!
//! A fourth reason used to live here — the instrument's market-hours blackout —
//! and it cancelled + deleted the row like the rest. But a closed market always
//! reopens, so it is the one reason that is *temporary*, and deleting the row
//! turned a nightly pause into a nightly loss of the setup.
//!
//! It is now [`HoldReason::MarketHours`](trade_control_core::hold::HoldReason),
//! which pulls the resting order and re-places it when the session resumes —
//! sharing the refcount with spread hours and news pauses so overlapping reasons
//! lift independently. This module keeps only the half a hold cannot do:
//! `CancelAndClose`, which flattens an already-filled position.
//!
//! (It had also been silently dead: it read the per-instrument
//! `blackout_windows` KV table, which has had no production writer since the
//! window deriver was retired.)
//!
//! Errors per-row are logged and skipped — the sweep MUST NOT abort
//! on a single account's failure, or one stale account would jam the
//! entire schedule.
//!
//! # Runtime-agnostic via the [`CronEnv`] seam
//!
//! Moved into `trade-control-cron` so both the wasm Cloudflare worker and the
//! native VM scheduler run the *same* sweep. The `&Env`-hidden broker
//! acquisition travels through the [`CronEnv`] seam; the caller opens the
//! [`StateStore`] and passes it in. The wasm-only broker-acquisition helpers
//! (`open_store`, `acquire_broker_for_account`, `resolve_broker_kind`) stay in
//! the wasm worker's `src/cron/sweep.rs` — they are the `EnvCronEnv` impl's
//! plumbing, not part of the sweep decision logic.

use chrono::{DateTime, Utc};
use trade_control_core::broker::{AttemptState, Broker};
use trade_control_core::intent::BlackoutCloseAction;
use trade_control_core::state::{EntryAttempt, StateStore};

// The pure sweep predicates live in `core` so the offline replay can share them
// (the `[[strategy_changes_in_both_replayer_and_worker]]` rule).
use trade_control_core::sweep_gate::{
    bar_expiry_due, breach_detected, market_blackout_due_symbol, update_adverse_extreme,
};

use crate::broker_handle::BrokerHandle;
use crate::seam::CronEnv;

/// Walk every still-tracked `EntryAttempt`. Cancel + delete any that
/// have expired or whose SL has been overtaken by current price.
///
/// `now` is threaded in (rather than calling `Utc::now()` here) so
/// the unit-testable sweep entry-point stays a pure function of
/// `(store, cron, now)`.
pub async fn sweep_pending_orders<S, C>(store: &S, cron: &C, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    let attempts = match store.list_all_entry_attempts().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("cron sweep: list_all_entry_attempts: {err}");
            return;
        }
    };

    tracing::info!("cron sweep: {} tracked attempts", attempts.len());

    for attempt in attempts {
        if let Err(err) = sweep_one(store, cron, &attempt, now).await {
            tracing::error!(
                "cron sweep[{}/{}/#{}]: {err}",
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.trade_id,
                attempt.attempt_no,
            );
        }
    }
}

/// Per-attempt sweep. Splits the reasons to act (expired, bar-expiry,
/// market blackout, SL breached, otherwise leave alone) and returns an
/// error string so the caller can log with row context.
async fn sweep_one<S, C>(
    store: &S,
    cron: &C,
    attempt: &EntryAttempt,
    now: DateTime<Utc>,
) -> Result<(), String>
where
    S: StateStore,
    C: CronEnv,
{
    if attempt.expires_at < now {
        cancel_and_delete(store, cron, attempt, "expired").await
    } else if bar_expiry_due(attempt.cancel_at, now) {
        // Bar-based expiry: the resting order has outlived its
        // `expiry_bars` window without filling. Cancel like an expiry
        // (no current-price fetch needed) but with a distinct reason so
        // it's greppable apart from the alert-window `expired` case.
        cancel_and_delete(store, cron, attempt, "bar-expiry").await
    } else if market_blackout_due_symbol(&attempt.instrument, now) {
        // Market-hours blackout. The RESTING order is not swept — it is held.
        //
        // A closed market always reopens, so this reason is temporary in a way
        // the sweep's other three are not: `expired`, `bar-expiry` and
        // `sl-breached` all mean the setup is dead, and cancel-and-delete is
        // right for them. A closed session means only "not now", so
        // `HoldReason::MarketHours` (v123) pulls the order and re-places it when
        // the session resumes. Deleting the `EntryAttempt` row here would
        // destroy the state that restore needs, and the operator would lose a
        // live setup every night rather than have it paused.
        //
        // What this branch still owns is the `CancelAndClose` half: flattening
        // an already-FILLED position over the closed session. That is a signed,
        // opt-in operator choice and the hold refcount deliberately never does
        // it — holds touch resting orders only, never a position.
        //
        // Taking this branch also stops the SL-breach check below from reading a
        // *stale* last-traded price across the gap.
        //
        // The predicate changed with this slice: it read the per-instrument
        // `blackout_windows` KV table, which has had no production writer since
        // the window deriver was retired — so `get_blackout_windows` always
        // returned empty and this branch could never fire, while the replay's
        // fill-sim read the baked mask and *did* block. Both sides read the
        // baked mask now (`[[strategy_changes_in_both_replayer_and_worker]]`).
        market_blackout_act(store, cron, attempt).await
    } else if let Some(sl) = attempt.stop_loss_price {
        // Only acquire a broker when there's a chance we'll need to
        // call `get_current_price` — i.e. the row carries an SL.
        let broker = cron.acquire_broker(attempt.account.as_deref()).await;
        match broker {
            Some(BrokerHandle::Oanda(b)) => maybe_breach_cancel(store, attempt, sl, &b, now).await,
            Some(BrokerHandle::TradeNation(b)) => {
                maybe_breach_cancel(store, attempt, sl, &b, now).await
            }
            Some(BrokerHandle::Ibkr(b)) => maybe_breach_cancel(store, attempt, sl, &b, now).await,
            None => Err("broker acquisition failed".into()),
        }
    } else {
        // No SL recorded (legacy row written before this PR) — let
        // the row expire naturally via its TTL.
        Ok(())
    }
}

/// Generic-over-broker helper so the OANDA / TN / IBKR paths share one body.
///
/// # The question this answers: "has price TRADED past the stop since placement"
///
/// Not "is spot past the stop right now". Those are different questions and the
/// difference is the whole point of this function's shape.
///
/// This used to read `get_current_price` — an **instantaneous spot quote** — and
/// hand it straight to [`breach_detected`]. Sampled on the ~900s upkeep loop,
/// that made the outcome depend on where the cron tick happened to land relative
/// to the price path: two identical price paths gave different answers, and any
/// excursion that opened and closed between two ticks was invisible. That is a
/// sampling lottery, not a rule, and it governed whether a REAL resting order
/// got cancelled.
///
/// So the tick now **folds** the quote into the row's persisted running adverse
/// extreme ([`EntryAttempt::adverse_extreme`]) and evaluates the breach against
/// **the extreme**. Once the extreme is past the stop it stays past, so the
/// decision is monotonic in the price path rather than in tick alignment. The
/// persistence is load-bearing — the excursion history cannot be recovered by
/// re-reading the quote, which is why this is a stored field rather than a
/// re-read.
///
/// A row with **no** extreme (legacy, or first observation) seeds from this
/// tick's quote and is judged on that. A missing extreme is never read as
/// "breached"; the seed value is a real observed price, so an order genuinely
/// already past its stop when first observed still cancels on that same tick.
///
/// The offline replay's matching half reads each bar's **adverse extreme**
/// (low for a Long, high for a Short) rather than its close — the bar-resolution
/// analogue of the same question. Close-sampling there was explicitly
/// **rejected**: it lets a bar trade clean through the stop and back inside with
/// the order surviving, which is exactly the case the rule exists to catch.
/// Both sides call the same shared [`breach_detected`] and now differ only in
/// resolution (`[[strategy_changes_in_both_replayer_and_worker]]`).
///
/// See `core::sweep_gate::update_adverse_extreme` for why the fixture corpus
/// **cannot** justify any of this — 854 orders truncated, zero outcomes changed
/// — and therefore why a green corpus is not grounds to simplify it away.
async fn maybe_breach_cancel<S: StateStore, B: Broker>(
    store: &S,
    attempt: &EntryAttempt,
    stop_loss: f64,
    broker: &B,
    _now: DateTime<Utc>,
) -> Result<(), String> {
    // Before anything else, see whether this attempt has FILLED since the last
    // tick, and if so write the broker's own trade id onto the row. See
    // [`snapshot_broker_trade_id`] — this is the only production path that
    // observes an ordinary fill.
    snapshot_broker_trade_id(store, attempt, broker).await;

    let current = broker
        .get_current_price(&attempt.instrument)
        .await
        .map_err(|err| format!("get_current_price: {err}"))?;

    // Fold first, judge second. Judging the spot reading and *then* recording it
    // would reintroduce the sampling lottery for exactly the tick that observes
    // the excursion.
    let extreme = update_adverse_extreme(attempt.direction, attempt.adverse_extreme, current);
    persist_adverse_extreme(store, attempt, extreme).await;

    if breach_detected(attempt.direction, extreme, stop_loss) {
        cancel_with_broker(broker, attempt, "sl-breached", extreme).await;
        delete_row(store, attempt).await;
        Ok(())
    } else {
        // Not breached — leave it alone for the next sweep, which will fold its
        // own quote into the extreme we just persisted.
        Ok(())
    }
}

/// Snapshot the broker's own trade id onto an attempt the first tick it is seen
/// to have **filled**.
///
/// # Why this lives in the sweep
///
/// `join_position_to_attempt` matches an open position back to the attempt that
/// opened it in two stages: exact on `broker_trade_id == position_id`, else a
/// coarse `(instrument, direction, account)` fallback that **cannot separate two
/// look-alike attempts** — a multi-shot re-entry, or two setups on one pair — and
/// returns whichever comes first. A mis-join hands the wrong trade's geometry to
/// whichever cron is amending that position's stop.
///
/// The exact stage was supposed to prevent that, but nothing populated it. The
/// only production writer was the retry gate, on the path where it looks up a
/// prior attempt and **rejects** a re-entry — code a trade that fills once and is
/// never re-fired never reaches. So `broker_trade_id` stayed `None` for the
/// position's whole life and every break-even / blackout amend went through the
/// coarse fallback, correct only by luck when just one position matched.
///
/// This is the missing observation. Nothing else watches a resting order become
/// a filled one: the break-even and blackout passes need the join to have already
/// worked, and the order-control re-price matches on `broker_order_id` against
/// orders that are by definition still **resting**. The sweep is the one pass
/// that visits every attempt with a broker already in hand.
///
/// # Cost
///
/// One extra `lookup_attempt_state` per tick per attempt that has **not yet**
/// been seen to fill, and **zero** for the rest of a position's life — the
/// `is_some` guard short-circuits before any I/O. So the steady-state cost of a
/// filled position is nothing, and the transient cost is bounded by the number of
/// orders actually resting. This mirrors the care `retry_gate` takes in skipping
/// its open-position backstop when there is nothing to correlate against.
///
/// # Failure convention
///
/// Fail-soft, like [`persist_adverse_extreme`]: log and carry on. Every outcome
/// other than `OpenPosition` is left alone rather than written as anything —
/// a `Pending` order has no trade id yet, and a lookup that errors tells us
/// nothing, so in both cases the right stored value is the one already there.
/// The next tick asks again.
async fn snapshot_broker_trade_id<S: StateStore, B: Broker>(
    store: &S,
    attempt: &EntryAttempt,
    broker: &B,
) {
    // Already snapshotted — the common case for the whole life of a filled
    // position. Costs nothing and, critically, never REWRITES: the stored id is
    // what the join keys on, so a later lookup must not be able to move it.
    if attempt.broker_trade_id.is_some() {
        return;
    }
    let state = broker
        .lookup_attempt_state(
            &attempt.instrument,
            &attempt.broker_order_id,
            attempt.broker_trade_id.as_deref(),
        )
        .await;
    let broker_trade_id = match state {
        Ok(AttemptState::OpenPosition { broker_trade_id }) => broker_trade_id,
        // Still resting, already closed, gone, or unknown — nothing to record.
        Ok(_) => return,
        Err(err) => {
            tracing::error!(
                "cron sweep lookup_attempt_state({}/{}/#{}): {err}",
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.trade_id,
                attempt.attempt_no,
            );
            return;
        }
    };
    if let Err(err) = store
        .set_entry_attempt_broker_trade_id(
            attempt.account.as_deref(),
            &attempt.trade_id,
            attempt.attempt_no,
            &broker_trade_id,
        )
        .await
    {
        tracing::error!(
            "cron sweep set_entry_attempt_broker_trade_id({}/{}/#{}): {err}",
            attempt.account.as_deref().unwrap_or("<global>"),
            attempt.trade_id,
            attempt.attempt_no,
        );
        return;
    }
    tracing::info!(
        "cron sweep: attempt {}/{}/#{} filled — snapshotted broker_trade_id={broker_trade_id} \
         (instrument={}, order_id={})",
        attempt.account.as_deref().unwrap_or("<global>"),
        attempt.trade_id,
        attempt.attempt_no,
        attempt.instrument,
        attempt.broker_order_id,
    );
}

/// Write an advanced running adverse extreme back onto the row — the half of
/// [`maybe_breach_cancel`] that makes the breach decision monotonic ACROSS ticks
/// rather than only within one.
///
/// Skipped when the value is unchanged, so a quiet market costs no writes.
///
/// **Fail-soft, deliberately.** A failed write means the next tick re-derives
/// from an older (or absent) extreme, which for that tick is exactly the
/// pre-fix behaviour — never a fabricated breach. The caller keeps using the
/// in-memory value, which is still the best reading available for THIS tick.
/// The alternative — propagating the error — would abandon a breach we have
/// already correctly detected because we could not write a note about it.
async fn persist_adverse_extreme<S: StateStore>(store: &S, attempt: &EntryAttempt, extreme: f64) {
    if attempt.adverse_extreme == Some(extreme) {
        return;
    }
    if let Err(err) = store
        .set_entry_attempt_adverse_extreme(
            attempt.account.as_deref(),
            &attempt.trade_id,
            attempt.attempt_no,
            extreme,
        )
        .await
    {
        tracing::error!(
            "cron sweep set_entry_attempt_adverse_extreme({}/{}/#{}): {err}",
            attempt.account.as_deref().unwrap_or("<global>"),
            attempt.trade_id,
            attempt.attempt_no,
        );
    }
}

/// Cancel via whichever broker the attempt's account belongs to,
/// then delete the row. Used by the expiry branch which doesn't
/// need a current-price fetch.
async fn cancel_and_delete<S: StateStore, C: CronEnv>(
    store: &S,
    cron: &C,
    attempt: &EntryAttempt,
    reason: &'static str,
) -> Result<(), String> {
    match cron.acquire_broker(attempt.account.as_deref()).await {
        Some(BrokerHandle::Oanda(b)) => {
            cancel_with_broker(&b, attempt, reason, f64::NAN).await;
        }
        Some(BrokerHandle::TradeNation(b)) => {
            cancel_with_broker(&b, attempt, reason, f64::NAN).await;
        }
        Some(BrokerHandle::Ibkr(b)) => {
            cancel_with_broker(&b, attempt, reason, f64::NAN).await;
        }
        None => return Err("broker acquisition failed".into()),
    }
    delete_row(store, attempt).await;
    Ok(())
}

/// Act on an attempt caught inside the market-hours blackout, per the row's
/// signed `blackout_close` policy.
///
/// **The resting order is not touched here** — [`HoldReason::MarketHours`] owns
/// it, and pulls/re-places it around the closed session. What remains is the one
/// thing a hold deliberately cannot do:
///
/// * [`BlackoutCloseAction::CancelResting`] (the default) — nothing. The hold
///   pulls the unfilled order; a *filled* position is left alone, because its SL
///   is the only thing that should ever close it (the
///   `[[veto_close_only_when_thesis_invalidated]]` rule).
/// * [`BlackoutCloseAction::CancelAndClose`] — market-close any open position on
///   the instrument. Opt-in only; the operator chose this at arm time because a
///   partly-formed setup carried through a closed session is not worth the
///   reopen-gap risk.
///
/// **The row is NOT deleted.** It was, when this branch also cancelled the
/// resting order — but the hold needs the row to restore from. Deleting it would
/// turn a nightly pause into a nightly loss of the setup. The row still retires
/// on its own clocks (`expires_at` / `cancel_at`), which the branches above this
/// one handle.
async fn market_blackout_act<S: StateStore, C: CronEnv>(
    _store: &S,
    cron: &C,
    attempt: &EntryAttempt,
) -> Result<(), String> {
    // CancelResting is the default and now means "the hold handles it" — so
    // there is nothing to do, and no reason to pay for a broker handle.
    if !matches!(attempt.blackout_close, BlackoutCloseAction::CancelAndClose) {
        return Ok(());
    }
    match cron.acquire_broker(attempt.account.as_deref()).await {
        Some(BrokerHandle::Oanda(b)) => blackout_close_position(&b, attempt).await,
        Some(BrokerHandle::TradeNation(b)) => blackout_close_position(&b, attempt).await,
        Some(BrokerHandle::Ibkr(b)) => blackout_close_position(&b, attempt).await,
        None => return Err("broker acquisition failed".into()),
    }
    Ok(())
}

/// Generic-over-broker body for [`market_blackout_act`]'s `CancelAndClose` arm:
/// flatten any open position on the instrument over the closed session.
async fn blackout_close_position<B: Broker>(broker: &B, attempt: &EntryAttempt) {
    let closed = broker.close_positions(&attempt.instrument).await;
    tracing::info!(
        "cron sweep market-blackout close: account={} trade_id={} attempt_no={} \
         instrument={} close={closed}",
        attempt.account.as_deref().unwrap_or("<global>"),
        attempt.trade_id,
        attempt.attempt_no,
        attempt.instrument,
    );
}

/// Wrap `Broker::cancel_order` with a single log line so per-row
/// outcomes are visible in CF logs. Cancel transient failures don't
/// abort — the row stays put for the next sweep to retry.
async fn cancel_with_broker<B: Broker>(
    broker: &B,
    attempt: &EntryAttempt,
    reason: &'static str,
    current_price: f64,
) {
    let account = attempt.account.as_deref().unwrap_or("");
    match broker.cancel_order(account, &attempt.broker_order_id).await {
        Ok(()) => tracing::info!(
            "cron sweep cancel ok: reason={reason} account={} trade_id={} attempt_no={} \
             instrument={} order_id={} current_price={current_price}",
            attempt.account.as_deref().unwrap_or("<global>"),
            attempt.trade_id,
            attempt.attempt_no,
            attempt.instrument,
            attempt.broker_order_id,
        ),
        Err(err) => tracing::error!(
            "cron sweep cancel failed (will retry next tick): reason={reason} \
             account={} trade_id={} attempt_no={} instrument={} order_id={} err={err}",
            attempt.account.as_deref().unwrap_or("<global>"),
            attempt.trade_id,
            attempt.attempt_no,
            attempt.instrument,
            attempt.broker_order_id,
        ),
    }
}

async fn delete_row<S: StateStore>(store: &S, attempt: &EntryAttempt) {
    if let Err(err) = store
        .delete_entry_attempt(
            attempt.account.as_deref(),
            &attempt.trade_id,
            attempt.attempt_no,
        )
        .await
    {
        tracing::error!(
            "cron sweep delete_entry_attempt({}/{}/#{}): {err}",
            attempt.account.as_deref().unwrap_or("<global>"),
            attempt.trade_id,
            attempt.attempt_no,
        );
    }
}

// The pure predicate unit tests (`breach_detected`, `bar_expiry_due`,
// `market_blackout_due_symbol`, `now_utc_minute_of_day`) live with the
// predicates in `trade_control_core::sweep_gate` — see its `#[cfg(test)] mod
// tests`.

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_core::dispatch_config::DispatchConfig;
    use trade_control_core::incoming::Verified;
    use trade_control_core::intent::Direction;
    use trade_control_core::state::MemStateStore;
    use trade_control_core::tick_bundle::TickBundle;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    /// A resting attempt on AUD/CHF whose own clocks are far in the future, so
    /// the only branch that can fire is the market-hours one.
    fn attempt(blackout_close: BlackoutCloseAction) -> EntryAttempt {
        EntryAttempt {
            trade_id: "t-mh".into(),
            account: None,
            instrument: "AUD/CHF".into(),
            attempt_no: 1,
            broker_order_id: "ord-1".into(),
            broker_trade_id: None,
            direction: Direction::Long,
            placed_at: ts("2026-07-10T20:00:00Z"),
            shell_time: ts("2026-07-10T20:00:00Z"),
            expires_at: ts("2026-07-20T00:00:00Z"),
            stop_loss_price: Some(0.5000),
            adverse_extreme: None,
            cancel_at: None,
            pip_size: Some(0.0001),
            blackout_close,
            breakeven: None,
            order_control: None,
            superseded: false,
        }
    }

    /// A `CronEnv` that PANICS on broker acquisition. Any branch that reaches a
    /// broker fails loudly rather than quietly exercising a stub — which is what
    /// makes "the sweep does nothing here" a real assertion rather than an
    /// absence of one.
    struct NoBrokerEnv;

    impl CronEnv for NoBrokerEnv {
        async fn acquire_broker(&self, _account: Option<&str>) -> Option<BrokerHandle> {
            panic!("the sweep must not reach a broker on this path")
        }
        async fn dispatch_config(&self, _verified: &Verified) -> DispatchConfig {
            unreachable!("not used by the sweep")
        }
        fn record_tick(&self, _bundle: TickBundle) {}
        fn signing_key(&self) -> Option<Vec<u8>> {
            None
        }
    }

    /// Saturday: AUD/CHF's market is shut (the real baked weekend halt).
    const MARKET_CLOSED: &str = "2026-07-11T12:00:00Z";

    /// THE SLICE-6 BEHAVIOUR: over a closed market the sweep does nothing — no
    /// broker call, and critically **the row survives**.
    ///
    /// The row is what `HoldReason::MarketHours` restores from on Monday.
    /// Deleting it (which this branch used to do) turns a weekend pause into a
    /// permanently lost setup.
    ///
    /// Mutation check: restore the `delete_row` call and this goes red; restore
    /// the `cancel_with_broker` call and `NoBrokerEnv` panics.
    #[test]
    fn a_closed_market_neither_cancels_nor_deletes_the_row() {
        let store = MemStateStore::new();
        let a = attempt(BlackoutCloseAction::CancelResting);
        pollster::block_on(async {
            store
                .record_entry_attempt(a.clone())
                .await
                .expect("seed the attempt");
            sweep_one(&store, &NoBrokerEnv, &a, ts(MARKET_CLOSED))
                .await
                .expect("a closed market is not an error");
            let rows = store
                .list_all_entry_attempts()
                .await
                .expect("list attempts");
            assert_eq!(
                rows.len(),
                1,
                "the row must survive — the hold restores from it when the market reopens",
            );
        });
    }

    /// The `CancelAndClose` opt-in still reaches a broker: flattening an
    /// already-FILLED position is the one thing a hold cannot do, so this half
    /// had to stay behind in the sweep.
    ///
    /// Asserted via the panic — a broker was acquired, which is the observable
    /// difference from the default path above.
    #[test]
    #[should_panic(expected = "must not reach a broker")]
    fn cancel_and_close_still_flattens_an_open_position() {
        let store = MemStateStore::new();
        let a = attempt(BlackoutCloseAction::CancelAndClose);
        pollster::block_on(sweep_one(&store, &NoBrokerEnv, &a, ts(MARKET_CLOSED))).ok();
    }

    // --- the running adverse extreme (SL-breach sweep) ----------------------
    //
    // These drive `sweep_one` / `maybe_breach_cancel` — the REAL entry points —
    // through a spy broker, not the pure predicate in `core::sweep_gate`. That
    // is deliberate: a mutation that judges the spot reading instead of the
    // extreme, or hardcodes a direction, leaves every pure-predicate test green.

    use std::cell::RefCell;
    use trade_control_core::broker::{
        AttemptState, CancelError, Candle, CandleError, CloseOutcome, EntryError, EntryRequest,
        Granularity, LookupError, OpenPosition, PendingOrder, Placement, Quote,
    };

    /// A broker that serves a SCRIPTED sequence of quotes — one per sweep tick —
    /// and records every cancel. The script is what makes an *excursion* (a price
    /// path, not a price) expressible, which is the whole subject here.
    struct ScriptedBroker {
        quotes: RefCell<std::collections::VecDeque<f64>>,
        cancels: RefCell<Vec<String>>,
        /// What `lookup_attempt_state` answers. `Unknown` (the default) is the
        /// "still resting, nothing to snapshot" case the extreme tests want.
        attempt_state: RefCell<Result<AttemptState, LookupError>>,
        /// How many times the sweep asked. Counting it is what makes "the id is
        /// written ONCE, not re-asked every tick" an assertion rather than a
        /// hope — the round-trip is the cost this fix has to justify.
        lookups: RefCell<usize>,
    }

    impl ScriptedBroker {
        fn new(quotes: &[f64]) -> Self {
            Self {
                quotes: RefCell::new(quotes.iter().copied().collect()),
                cancels: RefCell::new(Vec::new()),
                attempt_state: RefCell::new(Ok(AttemptState::Unknown)),
                lookups: RefCell::new(0),
            }
        }
        fn cancelled(&self) -> usize {
            self.cancels.borrow().len()
        }
        /// This attempt has filled, and the broker calls the position `id`.
        fn filled_as(mut self, id: &str) -> Self {
            self.attempt_state = RefCell::new(Ok(AttemptState::OpenPosition {
                broker_trade_id: id.into(),
            }));
            self
        }
        /// The lookup itself fails.
        fn lookup_fails(mut self) -> Self {
            self.attempt_state = RefCell::new(Err(LookupError::Transient));
            self
        }
        fn lookups(&self) -> usize {
            *self.lookups.borrow()
        }
    }

    impl Broker for ScriptedBroker {
        async fn get_quote(&self, _instrument: &str) -> Result<Quote, LookupError> {
            let p = self
                .quotes
                .borrow_mut()
                .pop_front()
                .expect("the test scripted a quote for every tick it drives");
            Ok(Quote { bid: p, ask: p })
        }
        async fn cancel_order(
            &self,
            _account_id: &str,
            broker_order_id: &str,
        ) -> Result<(), CancelError> {
            self.cancels.borrow_mut().push(broker_order_id.to_string());
            Ok(())
        }
        async fn place_entry(
            &self,
            _max_risk_pct: f64,
            _max_open_positions: u32,
            _req: &EntryRequest<'_>,
        ) -> Result<Placement, EntryError> {
            unreachable!("the sweep never places")
        }
        async fn close_positions(&self, _instrument: &str) -> CloseOutcome {
            unreachable!("the SL-breach arm never closes a position")
        }
        async fn cancel_pending_for_instrument(&self, _instrument: &str) -> usize {
            0
        }
        async fn lookup_attempt_state(
            &self,
            _instrument: &str,
            _broker_order_id: &str,
            _broker_trade_id: Option<&str>,
        ) -> Result<AttemptState, LookupError> {
            *self.lookups.borrow_mut() += 1;
            self.attempt_state.borrow().clone()
        }
        async fn list_open_positions(
            &self,
            _account_id: &str,
        ) -> Result<Vec<OpenPosition>, LookupError> {
            Ok(vec![])
        }
        async fn amend_stop(
            &self,
            _account_id: &str,
            _id: &str,
            _new_stop: f64,
        ) -> Result<(), trade_control_core::broker::AmendError> {
            Ok(())
        }
        async fn list_pending_orders(
            &self,
            _account_id: &str,
        ) -> Result<Vec<PendingOrder>, LookupError> {
            Ok(vec![])
        }
        async fn get_candles(
            &self,
            _instrument: &str,
            _granularity: Granularity,
            _since: DateTime<Utc>,
            _now: DateTime<Utc>,
        ) -> Result<Vec<Candle>, CandleError> {
            Ok(vec![])
        }
    }

    /// A resting attempt on a 24h-open instrument (so the market-hours branch
    /// cannot pre-empt the SL-breach one), with clocks far in the future.
    fn resting(direction: Direction, stop_loss: f64) -> EntryAttempt {
        EntryAttempt {
            direction,
            stop_loss_price: Some(stop_loss),
            instrument: "EUR/USD".into(),
            ..attempt(BlackoutCloseAction::CancelResting)
        }
    }

    /// Drive N sweep ticks against one store row, re-reading the row each tick
    /// exactly as the live loop does (it lists from the store every tick). This
    /// is what makes the PERSISTENCE observable: a mutation that never writes
    /// the extreme back is invisible to a single-tick test.
    fn drive(store: &MemStateStore, broker: &ScriptedBroker, row: &EntryAttempt, ticks: usize) {
        pollster::block_on(async {
            store
                .record_entry_attempt(row.clone())
                .await
                .expect("seed the attempt");
            for _ in 0..ticks {
                let Some(live) = store
                    .list_all_entry_attempts()
                    .await
                    .expect("list attempts")
                    .into_iter()
                    .find(|a| a.trade_id == row.trade_id)
                else {
                    break; // swept and deleted — the loop is over
                };
                let sl = live.stop_loss_price.expect("seeded with an SL");
                maybe_breach_cancel(store, &live, sl, broker, live.placed_at)
                    .await
                    .expect("the scripted broker never errors");
            }
        });
    }

    /// THE BUG, pinned from the entry point: a row that already carries an
    /// extreme past its stop must cancel on a tick whose SPOT has recovered.
    ///
    /// # Why the excursion is pre-seeded rather than scripted as tick 2
    ///
    /// A three-tick script `[above, through, recovered]` does NOT distinguish the
    /// two implementations, and a spot-reading mutation survives it: the tick that
    /// *sees* the excursion breaches under either reading (spot is past the stop
    /// right then) and the sweep is terminal, so tick 3 never runs. That version
    /// was written first and the mutation lived through it.
    ///
    /// The divergence only appears when the tick that ACTS is not the tick that
    /// OBSERVED — which is the live shape, not a contrivance: the extreme is
    /// persisted precisely so it survives across ticks and across a worker
    /// restart. Here the row arrives already carrying its excursion (as it would
    /// after a restart, or from a peer process) and the only quote this tick sees
    /// is a fully recovered one. Instantaneous spot says "leave it alone"; the
    /// remembered path says the thesis is already falsified.
    ///
    /// Mutations this kills: judging `current` instead of `extreme`.
    #[test]
    fn a_recovered_spot_still_cancels_when_the_stored_extreme_breached() {
        let store = MemStateStore::new();
        // The only quote is 1.1050 — comfortably ABOVE the 1.0950 stop.
        let broker = ScriptedBroker::new(&[1.1050]);
        let mut row = resting(Direction::Long, 1.0950);
        row.adverse_extreme = Some(1.0900); // the remembered excursion
        drive(&store, &broker, &row, 1);
        assert_eq!(
            broker.cancelled(),
            1,
            "spot has recovered, but price already traded past the stop — cancel",
        );
    }

    /// Direction mirror of the above: a Short's remembered spike ABOVE its stop,
    /// judged on a tick whose spot has dropped back well below it.
    ///
    /// Both signs are pinned because a `Direction::Long` hardcode is the mutation
    /// that survives a one-direction suite: a short's resting path sits below its
    /// SL, so a wrong-direction predicate still yields "not breached" and the same
    /// end state.
    #[test]
    fn a_recovered_short_still_cancels_when_the_stored_extreme_breached() {
        let store = MemStateStore::new();
        let broker = ScriptedBroker::new(&[1.0800]);
        let mut row = resting(Direction::Short, 1.1050);
        row.adverse_extreme = Some(1.1100); // spiked through, since recovered
        drive(&store, &broker, &row, 1);
        assert_eq!(broker.cancelled(), 1);
    }

    /// A multi-tick excursion, driven end to end: price dips through the stop and
    /// recovers over three ticks. Complements the pre-seeded pair above by
    /// exercising the FOLD as well as the read — the order must be gone, and gone
    /// once, not once per tick.
    ///
    /// Kills a direction-swapped fold (a Long tracking the max never breaches).
    #[test]
    fn a_long_excursion_over_several_ticks_cancels_exactly_once() {
        let store = MemStateStore::new();
        let broker = ScriptedBroker::new(&[1.1000, 1.0900, 1.1050]);
        let row = resting(Direction::Long, 1.0950);
        drive(&store, &broker, &row, 3);
        assert_eq!(broker.cancelled(), 1);
        assert!(
            pollster::block_on(store.list_all_entry_attempts())
                .expect("list attempts")
                .is_empty(),
            "a breach is terminal — the row goes with the order",
        );
    }

    /// Mirror of the multi-tick fold for a Short: the spike is the only adverse
    /// move, so a Long-hardcoded fold tracks the low (1.0800), never breaches,
    /// and this goes red.
    #[test]
    fn a_short_excursion_over_several_ticks_cancels_exactly_once() {
        let store = MemStateStore::new();
        let broker = ScriptedBroker::new(&[1.1000, 1.1100, 1.0800]);
        let row = resting(Direction::Short, 1.1050);
        drive(&store, &broker, &row, 3);
        assert_eq!(broker.cancelled(), 1);
    }

    /// The extreme must be PERSISTED, not recomputed per tick. Observed directly
    /// on the row rather than only through the cancel, so a mutation that drops
    /// the store write is caught at the write, not two ticks downstream.
    #[test]
    fn the_extreme_is_persisted_and_never_retracts() {
        let store = MemStateStore::new();
        // No tick breaches 1.0500, so the row survives all three and can be read.
        let broker = ScriptedBroker::new(&[1.1000, 1.0900, 1.1200]);
        let row = resting(Direction::Long, 1.0500);
        drive(&store, &broker, &row, 3);
        let saved = pollster::block_on(store.list_all_entry_attempts())
            .expect("list attempts")
            .into_iter()
            .find(|a| a.trade_id == row.trade_id)
            .expect("the row survives — nothing breached");
        assert_eq!(
            saved.adverse_extreme,
            Some(1.0900),
            "the worst price seen must be stored, and a better price must not retract it",
        );
        assert_eq!(broker.cancelled(), 0, "nothing reached the stop");
    }

    /// A missing extreme must NEVER be read as a breach. A legacy row (written
    /// before the field existed) seeds from its first observed quote and is judged
    /// on that real price — so a benign first tick leaves the order alone.
    #[test]
    fn a_legacy_row_with_no_extreme_is_not_treated_as_breached() {
        let store = MemStateStore::new();
        let broker = ScriptedBroker::new(&[1.1000]);
        let mut row = resting(Direction::Long, 1.0950);
        row.adverse_extreme = None; // explicit: this is the legacy shape
        drive(&store, &broker, &row, 1);
        assert_eq!(
            broker.cancelled(),
            0,
            "an absent extreme is 'not yet observed', never 'breached'",
        );
        let saved = pollster::block_on(store.list_all_entry_attempts())
            .expect("list attempts")
            .into_iter()
            .find(|a| a.trade_id == row.trade_id)
            .expect("the row survives");
        assert_eq!(
            saved.adverse_extreme,
            Some(1.1000),
            "the first observation seeds the extreme",
        );
    }

    /// Guard against over-correction the other way: an order whose stop is already
    /// blown on the very first tick that observes it must still cancel on that
    /// tick. Seeding must not buy a free pass.
    #[test]
    fn a_first_tick_already_past_the_stop_cancels_immediately() {
        let store = MemStateStore::new();
        let broker = ScriptedBroker::new(&[1.0900]);
        let row = resting(Direction::Long, 1.0950);
        drive(&store, &broker, &row, 1);
        assert_eq!(broker.cancelled(), 1);
        assert!(
            pollster::block_on(store.list_all_entry_attempts())
                .expect("list attempts")
                .is_empty(),
            "a breach is terminal — the row is deleted with the order",
        );
    }

    // --- snapshotting `broker_trade_id` when the fill is observed -----------
    //
    // The sweep is the only production path that watches a resting order become
    // a filled one, so it is where the id that makes
    // `join_position_to_attempt`'s exact stage usable gets written. See
    // `snapshot_broker_trade_id` for why the other four attempt-keyed crons
    // could not do it.
    //
    // Driven through `maybe_breach_cancel` (via `drive`) — the real caller —
    // and asserted on the STORED row, so a mutation that looks the state up and
    // drops it on the floor is caught at the write.

    /// Read one row back out of the store by `trade_id`.
    fn saved(store: &MemStateStore, trade_id: &str) -> EntryAttempt {
        pollster::block_on(store.list_all_entry_attempts())
            .expect("list attempts")
            .into_iter()
            .find(|a| a.trade_id == trade_id)
            .expect("the row survives this tick")
    }

    /// **THE SNAPSHOT HAPPENS AT ALL.** An attempt whose order has filled must
    /// come out of the tick carrying the broker's trade id — the id every
    /// downstream join keys on.
    ///
    /// Without this the field stays `None` for the position's entire life (the
    /// retry gate, its only other writer, runs only when it REJECTS a re-entry),
    /// so break-even and the blackout widen both fall through to the coarse
    /// `(instrument, direction, account)` key that cannot tell two look-alike
    /// attempts apart.
    #[test]
    fn a_filled_attempt_gets_the_brokers_trade_id_snapshotted_onto_it() {
        let store = MemStateStore::new();
        // A price nowhere near the stop, so the row survives to be read back.
        let broker = ScriptedBroker::new(&[1.1000]).filled_as("POS-REAL");
        let row = resting(Direction::Long, 1.0500);
        drive(&store, &broker, &row, 1);
        assert_eq!(
            saved(&store, &row.trade_id).broker_trade_id.as_deref(),
            Some("POS-REAL"),
            "the fill was observed but its trade id was never written to the row",
        );
    }

    /// An order still RESTING has no trade id yet, and must not acquire one.
    /// Writing anything here would be worse than writing nothing: the join
    /// treats a present id as authoritative, so a fabricated one aliases
    /// silently and permanently.
    #[test]
    fn a_still_resting_attempt_gets_no_trade_id() {
        let store = MemStateStore::new();
        // `Pending` — the default `Unknown` would also do, but this is the shape
        // an unfilled order actually reports.
        let mut broker = ScriptedBroker::new(&[1.1000]);
        broker.attempt_state = RefCell::new(Ok(AttemptState::Pending));
        let row = resting(Direction::Long, 1.0500);
        drive(&store, &broker, &row, 1);
        assert_eq!(
            saved(&store, &row.trade_id).broker_trade_id,
            None,
            "a resting order has not filled — nothing to snapshot",
        );
    }

    /// **WRITTEN ONCE, NOT REWRITTEN EVERY TICK.** Once the id is on the row the
    /// sweep must stop asking: the guard short-circuits before any I/O, so a
    /// filled position costs ZERO extra broker round-trips for the rest of its
    /// life. That bound is the cost argument for doing this in the sweep at all.
    ///
    /// Asserted on the LOOKUP COUNT, not just the stored value — a mutation that
    /// re-asks and happens to write the same id back is invisible to a value
    /// assertion, but it is exactly the per-tick cost this is meant to avoid.
    #[test]
    fn an_already_snapshotted_attempt_is_never_looked_up_again() {
        let store = MemStateStore::new();
        let broker = ScriptedBroker::new(&[1.1000, 1.1010, 1.1020]).filled_as("POS-REAL");
        let mut row = resting(Direction::Long, 1.0500);
        row.broker_trade_id = Some("POS-ALREADY".into());
        drive(&store, &broker, &row, 3);
        assert_eq!(
            broker.lookups(),
            0,
            "the row already carried an id; the sweep must not pay for a lookup",
        );
        assert_eq!(
            saved(&store, &row.trade_id).broker_trade_id.as_deref(),
            Some("POS-ALREADY"),
            "a stored trade id is the join's authority and must never be moved \
             by a later lookup",
        );
    }

    /// The transient case, and the reason the test above cannot pass merely
    /// because the sweep never looks anything up: an attempt with NO id yet is
    /// looked up on the tick it is still unfilled, and again once it fills —
    /// after which the guard takes over and the asking stops.
    #[test]
    fn an_unsnapshotted_attempt_is_looked_up_until_it_fills_then_stops() {
        let store = MemStateStore::new();
        let broker = ScriptedBroker::new(&[1.1000, 1.1010, 1.1020]).filled_as("POS-REAL");
        let row = resting(Direction::Long, 1.0500);
        drive(&store, &broker, &row, 3);
        assert_eq!(
            broker.lookups(),
            1,
            "tick 1 asks and gets the fill; ticks 2 and 3 must find the id \
             already on the row and not ask again",
        );
    }

    /// **A FAILED LOOKUP MUST NOT CORRUPT THE STORED VALUE.** A broker having a
    /// bad minute tells us nothing about whether the order filled, so the right
    /// stored value is the one already there. Fail-soft, like the adverse
    /// extreme beside it: log and let the next tick ask again.
    #[test]
    fn a_failed_lookup_leaves_the_row_untouched() {
        let store = MemStateStore::new();
        let broker = ScriptedBroker::new(&[1.1000]).lookup_fails();
        let row = resting(Direction::Long, 1.0500);
        drive(&store, &broker, &row, 1);
        assert_eq!(
            saved(&store, &row.trade_id).broker_trade_id,
            None,
            "a lookup that errored is not evidence of a fill",
        );
    }

    /// The same failure against a row that ALREADY carries an id: the existing
    /// value must survive untouched. This is the destructive half of the failure
    /// mode — clobbering a good id with `None` would silently drop the position
    /// back onto the aliasing fallback.
    #[test]
    fn a_failed_lookup_does_not_clear_an_existing_trade_id() {
        let store = MemStateStore::new();
        let broker = ScriptedBroker::new(&[1.1000]).lookup_fails();
        let mut row = resting(Direction::Long, 1.0500);
        row.broker_trade_id = Some("POS-KEEP".into());
        drive(&store, &broker, &row, 1);
        assert_eq!(
            saved(&store, &row.trade_id).broker_trade_id.as_deref(),
            Some("POS-KEEP"),
            "a broker failure must never cost a row the id it already had",
        );
    }

    /// The snapshot must not change what the sweep DOES. A filled attempt whose
    /// stored extreme is already past its stop still cancels on this tick — the
    /// lookup is an observation bolted onto the front, not a new gate.
    #[test]
    fn snapshotting_does_not_suppress_the_breach_cancel() {
        let store = MemStateStore::new();
        let broker = ScriptedBroker::new(&[1.1050]).filled_as("POS-REAL");
        let mut row = resting(Direction::Long, 1.0950);
        row.adverse_extreme = Some(1.0900); // already breached
        drive(&store, &broker, &row, 1);
        assert_eq!(
            broker.cancelled(),
            1,
            "the breach decision must be unaffected by the fill observation",
        );
    }

    /// Guard against over-correction: the sweep's genuinely TERMINAL reasons
    /// must still act. An expired row is dead — the hold has nothing to restore
    /// — so it still cancels and deletes, reaching a broker to do it.
    #[test]
    #[should_panic(expected = "must not reach a broker")]
    fn an_expired_row_is_still_swept() {
        let store = MemStateStore::new();
        let mut a = attempt(BlackoutCloseAction::CancelResting);
        a.expires_at = ts("2026-07-11T00:00:00Z"); // before MARKET_CLOSED
        pollster::block_on(sweep_one(&store, &NoBrokerEnv, &a, ts(MARKET_CLOSED))).ok();
    }

    /// THE CONSEQUENCE of a stale `broker_order_id`, pinned at the sweep.
    ///
    /// When `pending_lifecycle` restores a resting order it earlier cancelled,
    /// the broker answers with a NEW order id. If the `EntryAttempt` row is not
    /// re-pointed, this sweep — which dispatches **straight** on
    /// `attempt.broker_order_id`, with no `lookup_attempt_state` and no fallback
    /// — cancels an id the broker already discarded, while the live restored
    /// order rests on and can still fill through its breached stop.
    ///
    /// The restore itself lives in `core` and cannot be driven from here, so the
    /// re-point is applied through the real store method (which is what that
    /// restore calls) and the sweep is then driven end to end over the row the
    /// store actually holds. `core`'s
    /// `a_restored_order_updates_the_entry_attempts_broker_order_id` owns the
    /// other half — that the restore performs this write at all.
    ///
    /// Asserting the cancelled ID, not the cancel COUNT, is the point: a stale
    /// row still produces exactly one cancel, so a count-only assertion passes
    /// under the bug.
    #[test]
    fn the_sweep_cancels_the_restored_order_not_the_dead_one() {
        let store = MemStateStore::new();
        let broker = ScriptedBroker::new(&[1.0900]); // straight through the stop
        let mut row = resting(Direction::Long, 1.0950);
        row.broker_order_id = "order-cancelled-by-the-hold".into();

        pollster::block_on(async {
            store
                .record_entry_attempt(row.clone())
                .await
                .expect("seed the attempt");
            // What the restore does once the broker hands back the new id.
            store
                .set_entry_attempt_broker_order_id(
                    row.account.as_deref(),
                    &row.trade_id,
                    "order-cancelled-by-the-hold",
                    "order-restored",
                )
                .await
                .expect("re-point the attempt");
            let live = store
                .list_all_entry_attempts()
                .await
                .expect("list attempts")
                .into_iter()
                .find(|a| a.trade_id == row.trade_id)
                .expect("the row is still there");
            let sl = live.stop_loss_price.expect("seeded with an SL");
            maybe_breach_cancel(&store, &live, sl, &broker, live.placed_at)
                .await
                .expect("the scripted broker never errors");
        });

        assert_eq!(
            *broker.cancels.borrow(),
            vec!["order-restored".to_string()],
            "the sweep must cancel the order that is actually resting at the broker",
        );
    }
}
