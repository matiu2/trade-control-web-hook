//! Cron 1 — System 3 (Sub-plan 5): cancel resting *entry* orders during the
//! spread blackout and store their signed intent so the recovery watcher can
//! re-drive them.
//!
//! Runs right after the System-2 stop-widen in [`super::blackout_apply`], on
//! the same affected-account set. Per the master plan's "no classification"
//! rule, each found order's instrument spread is sampled via `get_quote` and
//! only an order on an *elevated* spread is cancelled — a major at ~5p resting
//! through the trough is left alone.
//!
//! ## Crash-safety ordering — store BEFORE cancel (load-bearing)
//!
//! The stored `cancelled_orders` list is the **source of truth** for restore.
//! We push the `CancelledOrder` onto the per-trade record (and set `applied`)
//! BEFORE calling `cancel_order`. A crash between the two leaves a stored
//! record for an order still live at the broker — the recovery watcher then
//! re-drives it, and the re-drive's own gates / the broker's pending state
//! bound the blast radius to a recoverable duplicate. The opposite ordering
//! (cancel then store) risks **losing the entry entirely** on a crash:
//! cancelled at the broker, no record, never restored. A lost wanted entry is
//! worse than a recoverable duplicate.
//!
//! ## Where the signed body comes from
//!
//! The cron finds a broker *pending order*, not a signed intent. `run_enter`
//! persists the raw signed body under an `order:{broker_order_id}` KV row on
//! every successful placement; we recover it here by `order_id`. An order with
//! NO stored body (placed before this feature, or its body TTL'd out) can't be
//! restored, so we **never cancel it** — leaving it resting is strictly safer
//! than cancelling something we can't put back.

use chrono::{DateTime, Duration, Utc};
use trade_control_core::broker::{Broker, PendingOrder};
use trade_control_core::incoming;
use trade_control_core::state::{CancelledOrder, SpreadBlackoutRecord, StateStore};
use worker::{Env, console_error, console_log};

use super::constants::BLACKOUT_BACKSTOP_SECONDS;
use super::sweep::{BrokerHandle, acquire_broker_for_account, open_store};
use crate::state::KvStateStore;

/// Cancel + store every resting entry order on the affected accounts whose
/// instrument spread is currently elevated. Affected accounts are sourced the
/// same way the widen does — the set of `account`s on the existing
/// `EntryAttempt` rows — so the two steps stay in lockstep.
pub async fn cancel_resting_orders(env: &Env, now: DateTime<Utc>) {
    let Some(store) = open_store(env) else {
        return;
    };
    let key = match crate::signing_key(env) {
        Some(k) => k,
        // No signing key ⇒ we can't re-parse a stored body to find its trade_id
        // (and the restore side couldn't re-verify it either). Skip the whole
        // cancel step rather than cancel orders we can't book for restore.
        None => {
            console_error!("blackout cancel: no signing key; skipping resting-order cancel");
            return;
        }
    };
    let attempts = match store.list_all_entry_attempts().await {
        Ok(v) => v,
        Err(err) => {
            console_error!("blackout cancel: list_all_entry_attempts: {err}");
            return;
        }
    };
    let mut accounts: Vec<Option<String>> = Vec::new();
    for a in &attempts {
        if !accounts.contains(&a.account) {
            accounts.push(a.account.clone());
        }
    }
    console_log!("blackout cancel: {} affected account(s)", accounts.len());
    for account in accounts {
        cancel_account(env, &store, &key, account.as_deref(), now).await;
    }
}

/// Cancel + store resting orders on one account. Per-order errors log + skip.
async fn cancel_account(
    env: &Env,
    store: &KvStateStore,
    key: &[u8],
    account: Option<&str>,
    now: DateTime<Utc>,
) {
    let Some(broker) = acquire_broker_for_account(env, account).await else {
        console_error!(
            "blackout cancel[{}]: broker acquisition failed; skipping account",
            account.unwrap_or("<global>"),
        );
        return;
    };
    let account_id = account.unwrap_or("");
    let pendings = match list_pending(&broker, account_id).await {
        Ok(p) => p,
        Err(err) => {
            console_error!(
                "blackout cancel[{}]: list_pending_orders: {err}",
                account.unwrap_or("<global>"),
            );
            return;
        }
    };
    for order in pendings {
        cancel_one(store, &broker, key, account, &order, now).await;
    }
}

/// Cancel + store a single resting order. Order of operations is crash-safe
/// (store, then cancel — see module docs).
async fn cancel_one(
    store: &KvStateStore,
    broker: &BrokerHandle,
    key: &[u8],
    account: Option<&str>,
    order: &PendingOrder,
    now: DateTime<Utc>,
) {
    let scope = account.unwrap_or("<global>");

    // Recover the signed body for THIS order. No body ⇒ we can't restore it,
    // so we must not cancel it (never strand an entry we can't put back).
    let signed_body = match store.get_order_body(&order.order_id).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            console_log!(
                "blackout cancel[{scope}]: no stored body for order {} — leaving it resting \
                 (can't restore what we can't re-parse)",
                order.order_id,
            );
            return;
        }
        Err(err) => {
            console_error!(
                "blackout cancel[{scope}]: get_order_body({}) failed: {err}; skip",
                order.order_id,
            );
            return;
        }
    };

    // Re-parse to recover the trade_id (record key) + pip_size (spread→pips).
    // A body that no longer verifies (window closed / tampered) is unusable —
    // skip without cancelling.
    let verified = match incoming::parse_and_verify(&signed_body, key, now) {
        Ok(v) => v,
        Err(err) => {
            console_log!(
                "blackout cancel[{scope}]: stored body for order {} won't verify ({err}); \
                 leaving it resting",
                order.order_id,
            );
            return;
        }
    };
    let trade_id = verified
        .intent
        .trade_id
        .clone()
        .unwrap_or_else(|| order.order_id.clone());
    let Some(pip_size) = verified
        .intent
        .pip_size
        .filter(|p| *p > 0.0 && p.is_finite())
    else {
        console_log!(
            "blackout cancel[{scope}]: order {} (trade {trade_id}) has no usable pip_size; \
             skip (won't classify spread with a wrong pip)",
            order.order_id,
        );
        return;
    };

    // Only cancel when THIS instrument's spread is actually blown.
    let quote = match get_quote(broker, &order.instrument).await {
        Ok(q) => q,
        Err(err) => {
            console_error!(
                "blackout cancel[{scope}]: get_quote({}) failed: {err:?}; skip trade {trade_id}",
                order.instrument,
            );
            return;
        }
    };
    let spread_pips = quote.spread() / pip_size;
    let threshold = crate::spread_blackout::elevated_threshold_pips(&order.instrument);
    if spread_pips <= threshold {
        console_log!(
            "blackout cancel[{scope}]: order {} ({}) spread {spread_pips:.1}p <= {threshold:.1}p \
             not elevated; leaving it resting",
            order.order_id,
            order.instrument,
        );
        return;
    }

    // STORE FIRST (crash-safe): merge a CancelledOrder onto the per-trade
    // record, set `applied`, preserve any Sub-plan-4 widened-stop originals.
    let existing = match store.get_spread_blackout_record(&trade_id).await {
        Ok(r) => r,
        Err(err) => {
            console_error!(
                "blackout cancel[{scope}]: get_record({trade_id}): {err}; skip (won't cancel \
                 without a durable record)",
            );
            return;
        }
    };
    let record = merge_cancelled_order(
        existing,
        &trade_id,
        &order.instrument,
        account,
        pip_size,
        CancelledOrder {
            order_id: order.order_id.clone(),
            signed_intent: signed_body,
        },
        now,
    );
    if let Err(err) = store
        .upsert_spread_blackout_record(&record, BLACKOUT_BACKSTOP_SECONDS)
        .await
    {
        console_error!(
            "blackout cancel[{scope}]: upsert_record({trade_id}) FAILED ({err}); NOT cancelling \
             (no durable record ⇒ would strand the order)",
        );
        return;
    }

    // Now cancel. A failure leaves the (idempotent) record in place; the
    // recovery re-drive of a still-live order is bounded by its own gates.
    match cancel(broker, account_id_of(account), &order.order_id).await {
        Ok(()) => console_log!(
            "blackout cancel[{scope}][{trade_id}]: cancelled resting {} order {} \
             (trigger={} spread={spread_pips:.1}p)",
            if order.is_stop { "stop" } else { "limit" },
            order.order_id,
            order.trigger,
        ),
        Err(err) => console_error!(
            "blackout cancel[{scope}][{trade_id}]: cancel order {} FAILED ({err}); record stays \
             (recovery re-drive is bounded by gates if the order was actually still live)",
            order.order_id,
        ),
    }
}

fn account_id_of(account: Option<&str>) -> &str {
    account.unwrap_or("")
}

/// Pure record merge: push `cancelled` onto a fresh-or-existing record, set
/// `applied = true`, and preserve any Sub-plan-4 `original_stops`. Unit-tested
/// in isolation (no KV / broker). Idempotency: a re-fire that re-cancels the
/// same order id de-dups so the list never grows an exact duplicate.
fn merge_cancelled_order(
    existing: Option<SpreadBlackoutRecord>,
    trade_id: &str,
    instrument: &str,
    account: Option<&str>,
    pip_size: f64,
    cancelled: CancelledOrder,
    now: DateTime<Utc>,
) -> SpreadBlackoutRecord {
    let mut record = existing.unwrap_or_else(|| SpreadBlackoutRecord {
        trade_id: trade_id.to_string(),
        instrument: instrument.to_string(),
        account: account.map(|s| s.to_string()),
        applied: false,
        opened_at: now,
        expires_at: now + Duration::seconds(BLACKOUT_BACKSTOP_SECONDS as i64),
        pip_size,
        original_stops: Vec::new(),
        cancelled_orders: Vec::new(),
    });
    record.applied = true;
    // Keep pip_size current if the prior record had none (Sub-plan-2-era row).
    if !(record.pip_size > 0.0 && record.pip_size.is_finite()) {
        record.pip_size = pip_size;
    }
    if !record
        .cancelled_orders
        .iter()
        .any(|c| c.order_id == cancelled.order_id)
    {
        record.cancelled_orders.push(cancelled);
    }
    record
}

async fn list_pending(
    broker: &BrokerHandle,
    account_id: &str,
) -> Result<Vec<PendingOrder>, String> {
    let res = match broker {
        BrokerHandle::Oanda(b) => b.list_pending_orders(account_id).await,
        BrokerHandle::TradeNation(b) => b.list_pending_orders(account_id).await,
    };
    res.map_err(|e| format!("{e:?}"))
}

async fn get_quote(
    broker: &BrokerHandle,
    instrument: &str,
) -> Result<trade_control_core::broker::Quote, trade_control_core::broker::LookupError> {
    match broker {
        BrokerHandle::Oanda(b) => b.get_quote(instrument).await,
        BrokerHandle::TradeNation(b) => b.get_quote(instrument).await,
    }
}

async fn cancel(
    broker: &BrokerHandle,
    account_id: &str,
    order_id: &str,
) -> Result<(), trade_control_core::broker::CancelError> {
    match broker {
        BrokerHandle::Oanda(b) => b.cancel_order(account_id, order_id).await,
        BrokerHandle::TradeNation(b) => b.cancel_order(account_id, order_id).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    fn cancelled(order_id: &str) -> CancelledOrder {
        CancelledOrder {
            order_id: order_id.into(),
            signed_intent: format!("id: {order_id}\nsig: v1-sig.xxx\n"),
        }
    }

    #[test]
    fn merge_onto_fresh_record_sets_applied_and_pushes() {
        let rec = merge_cancelled_order(
            None,
            "hs-eur-nzd-c1e0f25b",
            "EUR_NZD",
            Some("reversals"),
            0.0001,
            cancelled("ORD-1"),
            ts("2026-03-12T21:05:00Z"),
        );
        assert!(rec.applied, "cancel is a real broker mutation");
        assert_eq!(rec.trade_id, "hs-eur-nzd-c1e0f25b");
        assert_eq!(rec.instrument, "EUR_NZD");
        assert_eq!(rec.account.as_deref(), Some("reversals"));
        assert_eq!(rec.pip_size, 0.0001);
        assert_eq!(rec.cancelled_orders.len(), 1);
        assert_eq!(rec.cancelled_orders[0].order_id, "ORD-1");
        assert!(rec.original_stops.is_empty());
    }

    #[test]
    fn merge_preserves_existing_widened_stops_subplan4_coexistence() {
        use trade_control_core::state::RememberedStop;
        let existing = SpreadBlackoutRecord {
            trade_id: "t1".into(),
            instrument: "EUR_NZD".into(),
            account: Some("reversals".into()),
            applied: true, // Sub-plan 4 already widened a stop on this trade
            opened_at: ts("2026-03-12T21:05:00Z"),
            expires_at: ts("2026-03-13T00:05:00Z"),
            pip_size: 0.0001,
            original_stops: vec![RememberedStop {
                position_or_order_id: "POS-9".into(),
                original_stop: 1.8000,
            }],
            cancelled_orders: Vec::new(),
        };
        let rec = merge_cancelled_order(
            Some(existing),
            "t1",
            "EUR_NZD",
            Some("reversals"),
            0.0001,
            cancelled("ORD-2"),
            ts("2026-03-12T21:10:00Z"),
        );
        // Sub-plan-4 stops untouched; Sub-plan-5 order added alongside.
        assert_eq!(rec.original_stops.len(), 1);
        assert_eq!(rec.original_stops[0].position_or_order_id, "POS-9");
        assert_eq!(rec.cancelled_orders.len(), 1);
        assert_eq!(rec.cancelled_orders[0].order_id, "ORD-2");
        assert!(rec.applied);
    }

    #[test]
    fn merge_dedups_same_order_id_on_refire() {
        let existing = merge_cancelled_order(
            None,
            "t1",
            "EUR_NZD",
            Some("reversals"),
            0.0001,
            cancelled("ORD-1"),
            ts("2026-03-12T21:05:00Z"),
        );
        // Cron re-fires (CF double-deliver) and re-cancels the same order id.
        let rec = merge_cancelled_order(
            Some(existing),
            "t1",
            "EUR_NZD",
            Some("reversals"),
            0.0001,
            cancelled("ORD-1"),
            ts("2026-03-12T21:06:00Z"),
        );
        assert_eq!(rec.cancelled_orders.len(), 1, "no exact-duplicate growth");
    }

    #[test]
    fn merge_backfills_pip_on_legacy_record_without_one() {
        let existing = SpreadBlackoutRecord {
            trade_id: "t1".into(),
            instrument: "EUR_NZD".into(),
            account: None,
            applied: false,
            opened_at: ts("2026-03-12T21:05:00Z"),
            expires_at: ts("2026-03-13T00:05:00Z"),
            pip_size: 0.0, // Sub-plan-2-era row
            original_stops: Vec::new(),
            cancelled_orders: Vec::new(),
        };
        let rec = merge_cancelled_order(
            Some(existing),
            "t1",
            "EUR_NZD",
            None,
            0.0001,
            cancelled("ORD-1"),
            ts("2026-03-12T21:10:00Z"),
        );
        assert_eq!(rec.pip_size, 0.0001, "backfilled from the cancel's pip");
    }
}
