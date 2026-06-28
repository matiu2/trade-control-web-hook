//! SL-breach + expiry sweep of pending `EntryAttempt` rows.
//!
//! Runs on a cron schedule (see [`crate::cron`]). For each tracked
//! `EntryAttempt` either:
//!
//! * its `expires_at` has passed → cancel + delete (the alert window
//!   itself is dead, so the still-pending order should be too); or
//! * its `stop_loss_price` has been overtaken by current price → the
//!   setup is invalidated before it ever filled, cancel + delete.
//!
//! Errors per-row are logged and skipped — the sweep MUST NOT abort
//! on a single account's failure, or one stale account would jam the
//! entire schedule.

use chrono::{DateTime, Utc};
use trade_control_core::broker::Broker;
use trade_control_core::intent::{BlackoutCloseAction, BrokerKind};
use trade_control_core::state::{EntryAttempt, StateStore};
use worker::Env;

// The pure sweep predicates now live in `core` so the offline replay can share
// them (the `[[strategy_changes_in_both_replayer_and_worker]]` rule). Imported
// here so every call site in this file stays byte-unchanged.
use trade_control_core::sweep_gate::{bar_expiry_due, breach_detected, market_blackout_due};

use crate::state::KvStateStore;

/// Walk every still-tracked `EntryAttempt`. Cancel + delete any that
/// have expired or whose SL has been overtaken by current price.
///
/// `now` is threaded in (rather than calling `Utc::now()` here) so
/// the unit-testable sweep entry-point stays a pure function of
/// `(env, now)`.
pub async fn sweep_pending_orders(env: &Env, now: DateTime<Utc>) {
    let store = match open_store(env) {
        Some(s) => s,
        None => return,
    };

    let attempts = match store.list_all_entry_attempts().await {
        Ok(v) => v,
        Err(err) => {
            rlog_err!("cron sweep: list_all_entry_attempts: {err}");
            return;
        }
    };

    rlog!("cron sweep: {} tracked attempts", attempts.len());

    for attempt in attempts {
        if let Err(err) = sweep_one(env, &store, &attempt, now).await {
            rlog_err!(
                "cron sweep[{}/{}/#{}]: {err}",
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.trade_id,
                attempt.attempt_no,
            );
        }
    }
}

/// Per-attempt sweep. Splits the three reasons to act (expired, SL
/// breached, otherwise leave alone) and returns an error string so
/// the caller can log with row context.
async fn sweep_one(
    env: &Env,
    store: &KvStateStore,
    attempt: &EntryAttempt,
    now: DateTime<Utc>,
) -> Result<(), String> {
    if attempt.expires_at < now {
        cancel_and_delete(env, store, attempt, "expired").await
    } else if bar_expiry_due(attempt.cancel_at, now) {
        // Bar-based expiry: the resting order has outlived its
        // `expiry_bars` window without filling. Cancel like an expiry
        // (no current-price fetch needed) but with a distinct reason so
        // it's greppable apart from the alert-window `expired` case.
        cancel_and_delete(env, store, attempt, "bar-expiry").await
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
        market_blackout_act(env, store, attempt).await
    } else if let Some(sl) = attempt.stop_loss_price {
        // Only acquire a broker when there's a chance we'll need to
        // call `get_current_price` — i.e. the row carries an SL.
        let broker = acquire_broker_for_attempt(env, attempt).await;
        match broker {
            Some(BrokerHandle::Oanda(b)) => {
                maybe_breach_cancel(env, store, attempt, sl, &b, now).await
            }
            Some(BrokerHandle::TradeNation(b)) => {
                maybe_breach_cancel(env, store, attempt, sl, &b, now).await
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
async fn maybe_breach_cancel<B: Broker>(
    env: &Env,
    store: &KvStateStore,
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
        let _ = env;
        Ok(())
    }
}

/// Cancel via whichever broker the attempt's account belongs to,
/// then delete the row. Used by the expiry branch which doesn't
/// need a current-price fetch.
async fn cancel_and_delete(
    env: &Env,
    store: &KvStateStore,
    attempt: &EntryAttempt,
    reason: &'static str,
) -> Result<(), String> {
    match acquire_broker_for_attempt(env, attempt).await {
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
async fn market_blackout_act(
    env: &Env,
    store: &KvStateStore,
    attempt: &EntryAttempt,
) -> Result<(), String> {
    match acquire_broker_for_attempt(env, attempt).await {
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
        rlog!(
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
        Ok(()) => rlog!(
            "cron sweep cancel ok: reason={reason} account={} trade_id={} attempt_no={} \
             instrument={} order_id={} current_price={current_price}",
            attempt.account.as_deref().unwrap_or("<global>"),
            attempt.trade_id,
            attempt.attempt_no,
            attempt.instrument,
            attempt.broker_order_id,
        ),
        Err(err) => rlog_err!(
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

async fn delete_row(store: &KvStateStore, attempt: &EntryAttempt) {
    if let Err(err) = store
        .delete_entry_attempt(
            attempt.account.as_deref(),
            &attempt.trade_id,
            attempt.attempt_no,
        )
        .await
    {
        rlog_err!(
            "cron sweep delete_entry_attempt({}/{}/#{}): {err}",
            attempt.account.as_deref().unwrap_or("<global>"),
            attempt.trade_id,
            attempt.attempt_no,
        );
    }
}

/// Open the KV-backed state store. Shared with the spread-blackout
/// cron steps (`blackout_apply`, `blackout_watch`).
pub(crate) fn open_store(env: &Env) -> Option<KvStateStore> {
    match env.kv(crate::KV_NAMESPACE) {
        Ok(kv) => Some(KvStateStore::new(kv)),
        Err(err) => {
            rlog_err!("cron sweep: KV binding missing: {err:?}");
            None
        }
    }
}

/// One enum so the dispatcher can return either broker type without
/// boxing across the async boundary (which `impl Trait` precludes).
/// Shared with the spread-recovery watcher, which calls `get_quote`
/// through the same per-broker match.
pub(crate) enum BrokerHandle {
    Oanda(broker_oanda::OandaBroker),
    TradeNation(crate::tradenation_adapter::TradeNationAdapter),
}

/// Pick a broker for the attempt's account. Thin wrapper over
/// [`acquire_broker_for_account`].
async fn acquire_broker_for_attempt(env: &Env, attempt: &EntryAttempt) -> Option<BrokerHandle> {
    acquire_broker_for_account(env, attempt.account.as_deref()).await
}

/// Pick a broker for `account`. `None` → worker-global OANDA (matches
/// the existing fetch-path default). `Some(name)` → the account's
/// broker kind, looked up from metadata. Shared between the order
/// sweep and the spread-recovery watcher.
pub(crate) async fn acquire_broker_for_account(
    env: &Env,
    account: Option<&str>,
) -> Option<BrokerHandle> {
    let broker_kind = resolve_broker_kind(env, account).await?;
    match broker_kind {
        BrokerKind::Oanda => crate::acquire_oanda_broker(env, account)
            .await
            .map(BrokerHandle::Oanda),
        BrokerKind::TradeNation => crate::acquire_tn_broker(env, account)
            .await
            .map(|b| BrokerHandle::TradeNation(crate::tradenation_adapter::TradeNationAdapter(b))),
    }
}

/// Resolve broker kind from the account metadata. Returns:
/// * `Some(Oanda)` when the attempt is unnamed (worker-global) — the
///   fetch-path treats `account: None` as the global OANDA account.
/// * `Some(kind)` on a successful metadata lookup.
/// * `None` on the native test target, or when KV / metadata lookup
///   fails — `None` is logged by the caller and the row is skipped
///   rather than silently misrouted to the wrong broker. PR A had a
///   non-wasm `BrokerKind::Oanda` fallback here that would have routed
///   TN accounts to OANDA in tests.
async fn resolve_broker_kind(env: &Env, account: Option<&str>) -> Option<BrokerKind> {
    let Some(name) = account else {
        return Some(BrokerKind::Oanda);
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (env, name);
        None
    }
    #[cfg(target_arch = "wasm32")]
    {
        use trade_control_core::account::MetadataStore;
        let kv = match env.kv(crate::KV_NAMESPACE) {
            Ok(kv) => kv,
            Err(err) => {
                rlog_err!("cron sweep[{name}]: KV binding missing: {err:?}");
                return None;
            }
        };
        let metadata = crate::accounts::KvMetadataStore::new(kv);
        match metadata.get(name).await {
            Ok(m) => Some(m.broker),
            Err(err) => {
                rlog_err!("cron sweep[{name}]: metadata lookup failed: {err}");
                None
            }
        }
    }
}

// The pure predicate unit tests (`breach_detected`, `bar_expiry_due`,
// `market_blackout_due`, `now_utc_minute_of_day`) moved with the predicates to
// `trade_control_core::sweep_gate` — see its `#[cfg(test)] mod tests`.
