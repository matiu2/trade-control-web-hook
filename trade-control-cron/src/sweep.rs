//! SL-breach + expiry sweep of pending `EntryAttempt` rows.
//!
//! Runs on a cron schedule. For each tracked `EntryAttempt` either:
//!
//! * its `expires_at` has passed → cancel + delete (the alert window
//!   itself is dead, so the still-pending order should be too); or
//! * its `cancel_at` (bar-based expiry) has passed → cancel + delete; or
//! * the instrument's market-hours blackout window has opened → cancel
//!   (and optionally close) per the row's `blackout_close` policy; or
//! * its `stop_loss_price` has been overtaken by current price → the
//!   setup is invalidated before it ever filled, cancel + delete.
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
use trade_control_core::sweep_gate::{bar_expiry_due, breach_detected, market_blackout_due};

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
    } else if market_blackout_due(
        &store
            .get_blackout_windows(&attempt.instrument)
            .await
            .unwrap_or_default(),
        now,
    ) {
        // Market-hours blackout: this order is still resting as the
        // instrument's daily close→open gap opens. Leaving it to rest is
        // exactly the incident — it would trigger on the reopen gap. Pull
        // it now, per the operator's `blackout_close` policy. Runs BEFORE
        // the SL-breach branch: across a closed session the last-traded
        // price is stale, so we must not let a stale-price SL check (or its
        // absence) decide; the closed market itself is the trigger.
        //
        // A KV read error fails open (`unwrap_or_default` ⇒ empty ⇒ not
        // due), matching the reject gate — a transient hiccup must not pull
        // a legitimate resting order.
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

/// Act on a resting order caught inside the market-hours blackout window,
/// per the row's `blackout_close` policy:
///
/// * [`BlackoutCloseAction::CancelResting`] (default, the incident fix) —
///   cancel the unfilled resting order only. NEVER closes a position: if the
///   order already filled, the cancel is a no-op on the broker and the filled
///   position is left untouched (its SL is the only thing that should ever
///   close it — see the `[[veto_close_only_when_thesis_invalidated]]` rule).
/// * [`BlackoutCloseAction::CancelAndClose`] — also market-close any open
///   position on the instrument. Opt-in only; the operator chose this at arm
///   time because a partly-formed setup carried through a closed session is
///   not worth the reopen-gap risk.
///
/// Either way the row is deleted afterwards so the next sweep doesn't
/// re-process it. The cancel reason string is distinct (`market-blackout`)
/// so it's greppable in CF logs apart from `expired` / `bar-expiry` /
/// `sl-breached`.
async fn market_blackout_act<S: StateStore, C: CronEnv>(
    store: &S,
    cron: &C,
    attempt: &EntryAttempt,
) -> Result<(), String> {
    match cron.acquire_broker(attempt.account.as_deref()).await {
        Some(BrokerHandle::Oanda(b)) => blackout_cancel_close(&b, attempt).await,
        Some(BrokerHandle::TradeNation(b)) => blackout_cancel_close(&b, attempt).await,
        None => return Err("broker acquisition failed".into()),
    }
    delete_row(store, attempt).await;
    Ok(())
}

/// Generic-over-broker body for [`market_blackout_act`]. Always cancels the
/// resting order; additionally closes positions only on `CancelAndClose`.
async fn blackout_cancel_close<B: Broker>(broker: &B, attempt: &EntryAttempt) {
    // Always cancel the resting order first.
    cancel_with_broker(broker, attempt, "market-blackout", f64::NAN).await;

    // CancelAndClose additionally flattens any open position on the
    // instrument. CancelResting (the default) stops here — it must never
    // close a position.
    if matches!(attempt.blackout_close, BlackoutCloseAction::CancelAndClose) {
        let closed = broker.close_positions(&attempt.instrument).await;
        tracing::info!(
            "cron sweep market-blackout close: account={} trade_id={} attempt_no={} \
             instrument={} closed_any={closed}",
            attempt.account.as_deref().unwrap_or("<global>"),
            attempt.trade_id,
            attempt.attempt_no,
            attempt.instrument,
        );
    }
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
// `market_blackout_due`, `now_utc_minute_of_day`) live with the predicates in
// `trade_control_core::sweep_gate` — see its `#[cfg(test)] mod tests`.
