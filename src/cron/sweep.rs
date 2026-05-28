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
use trade_control_core::intent::{BrokerKind, Direction};
use trade_control_core::state::{EntryAttempt, StateStore};
use worker::{Env, console_error, console_log};

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
            console_error!("cron sweep: list_all_entry_attempts: {err}");
            return;
        }
    };

    console_log!("cron sweep: {} tracked attempts", attempts.len());

    for attempt in attempts {
        if let Err(err) = sweep_one(env, &store, &attempt, now).await {
            console_error!(
                "cron sweep[{}/{}/#{}]: {err}",
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.trade_id,
                attempt.attempt_no,
            );
        }
    }
}

/// Pure breach predicate. Long is breached when current ≤ SL; short
/// when current ≥ SL. Kept tiny and pure so it's trivially testable.
pub fn breach_detected(direction: Direction, current_price: f64, stop_loss: f64) -> bool {
    match direction {
        Direction::Long => current_price <= stop_loss,
        Direction::Short => current_price >= stop_loss,
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
        Ok(()) => console_log!(
            "cron sweep cancel ok: reason={reason} account={} trade_id={} attempt_no={} \
             instrument={} order_id={} current_price={current_price}",
            attempt.account.as_deref().unwrap_or("<global>"),
            attempt.trade_id,
            attempt.attempt_no,
            attempt.instrument,
            attempt.broker_order_id,
        ),
        Err(err) => console_error!(
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
        console_error!(
            "cron sweep delete_entry_attempt({}/{}/#{}): {err}",
            attempt.account.as_deref().unwrap_or("<global>"),
            attempt.trade_id,
            attempt.attempt_no,
        );
    }
}

fn open_store(env: &Env) -> Option<KvStateStore> {
    match env.kv(crate::KV_NAMESPACE) {
        Ok(kv) => Some(KvStateStore::new(kv)),
        Err(err) => {
            console_error!("cron sweep: KV binding missing: {err:?}");
            None
        }
    }
}

/// One enum so the dispatcher can return either broker type without
/// boxing across the async boundary (which `impl Trait` precludes).
enum BrokerHandle {
    Oanda(broker_oanda::OandaBroker),
    TradeNation(crate::tradenation_adapter::TradeNationAdapter),
}

/// Pick a broker for the attempt's account. `None` → worker-global
/// OANDA (matches the existing fetch-path default). `Some(name)` →
/// the account's broker kind, looked up from metadata.
async fn acquire_broker_for_attempt(env: &Env, attempt: &EntryAttempt) -> Option<BrokerHandle> {
    let account = attempt.account.as_deref();
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
                console_error!("cron sweep[{name}]: KV binding missing: {err:?}");
                return None;
            }
        };
        let metadata = crate::accounts::KvMetadataStore::new(kv);
        match metadata.get(name).await {
            Ok(m) => Some(m.broker),
            Err(err) => {
                console_error!("cron sweep[{name}]: metadata lookup failed: {err}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_breach_when_price_at_or_below_sl() {
        assert!(breach_detected(Direction::Long, 1.0500, 1.0500));
        assert!(breach_detected(Direction::Long, 1.0499, 1.0500));
        assert!(!breach_detected(Direction::Long, 1.0501, 1.0500));
    }

    #[test]
    fn short_breach_when_price_at_or_above_sl() {
        assert!(breach_detected(Direction::Short, 1.0500, 1.0500));
        assert!(breach_detected(Direction::Short, 1.0501, 1.0500));
        assert!(!breach_detected(Direction::Short, 1.0499, 1.0500));
    }
}
