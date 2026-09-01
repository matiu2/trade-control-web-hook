//! SL-breach + expiry sweep of pending `EntryAttempt` rows.
//!
//! Runs on a cron schedule. For each tracked `EntryAttempt` either:
//!
//! * its `expires_at` has passed → cancel + delete (the alert window
//!   itself is dead, so the still-pending order should be too); or
//! * its `cancel_at` (bar-based expiry) has passed → cancel + delete; or
//! * its `stop_loss_price` has been overtaken by current price → the
//!   setup is invalidated before it ever filled, cancel + delete.
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
use trade_control_core::broker::Broker;
use trade_control_core::intent::BlackoutCloseAction;
use trade_control_core::state::{EntryAttempt, StateStore};

// The pure sweep predicates live in `core` so the offline replay can share them
// (the `[[strategy_changes_in_both_replayer_and_worker]]` rule).
use trade_control_core::sweep_gate::{bar_expiry_due, breach_detected, market_blackout_due_symbol};

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
            None => Err("broker acquisition failed".into()),
        }
    } else {
        // No SL recorded (legacy row written before this PR) — let
        // the row expire naturally via its TTL.
        Ok(())
    }
}

/// Generic-over-broker helper so the OANDA / TN paths share one body.
async fn maybe_breach_cancel<S: StateStore, B: Broker>(
    store: &S,
    attempt: &EntryAttempt,
    stop_loss: f64,
    broker: &B,
    _now: DateTime<Utc>,
) -> Result<(), String> {
    let current = broker
        .get_current_price(&attempt.instrument)
        .await
        .map_err(|err| format!("get_current_price: {err}"))?;
    if breach_detected(attempt.direction, current, stop_loss) {
        cancel_with_broker(broker, attempt, "sl-breached", current).await;
        delete_row(store, attempt).await;
        Ok(())
    } else {
        // Not breached — leave it alone for the next sweep.
        Ok(())
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
            cancel_at: None,
            pip_size: Some(0.0001),
            blackout_close,
            breakeven: None,
            order_control: None,
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
}
