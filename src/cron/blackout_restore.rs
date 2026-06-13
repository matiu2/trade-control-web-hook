//! Cron 2 — System 3 (Sub-plan 5): re-drive resting entry orders cancelled by
//! the blackout once the spread has recovered (or the backstop fires).
//!
//! For each `CancelledOrder` on a per-trade [`SpreadBlackoutRecord`], we:
//!
//! 1. **Reconstruct an authentic `Verified`** from the stored raw signed body
//!    via [`incoming::parse_and_verify`] — the *only* constructor of a
//!    `Verified`, and the reason the apply side stores the whole signed body
//!    rather than a deserialised `Intent`. The signing key is the same secret
//!    the HTTP path verifies with ([`crate::signing_key`]). A body that no
//!    longer verifies (`Expired` / `StaleShellTime` — the alert window closed
//!    during a long blackout) is **dropped with a log, never placed**.
//! 2. **Cheap fill-side pre-check** via the pure
//!    [`blackout_recreate::restore_plan`] using a fresh quote: a placeable stop
//!    or limit re-drives; an overrun stop with `on_too_close=skip` is dropped
//!    without a broker round-trip; a stale limit is dropped (limits are
//!    themselves a fallback).
//! 3. **Re-drive through [`crate::run_enter`]** — NOT `place_entry` directly —
//!    so sizing at the actual fill reference, the prep/veto/cooldown/allow_entry
//!    gates, AND the Sub-plan-0 `on_too_close` fallback all apply for free.
//!
//! ## Seen-id / retry-gate interaction (load-bearing — see CLAUDE.md
//! "Replay protection scope")
//!
//! A re-drive is the **SAME intended entry**, not a fresh multi-shot re-entry:
//!
//! - **Seen-id dedup is OUTSIDE `run_enter`** (it lives in the HTTP dispatcher,
//!   `src/lib.rs` fetch path). The cron calls `run_enter` **directly**, so it
//!   never hits `is_seen`. This is exactly right: the original `enter` fire
//!   succeeded (the order was placed + rested) so its id is already
//!   `mark_seen`'d — going through the dispatcher would 409. We therefore also
//!   **do NOT call `mark_seen`** for the re-drive (`run_enter`'s `ActionResult`
//!   is logged, never routed through `record_dispatcher_outcome`).
//! - **Retry-gate slot.** For a single-shot order (`max_retries == Static(0)`,
//!   the common resting-order case) `run_enter` skips the retry gate and writes
//!   no `EntryAttempt` — the re-drive consumes no slot. For a multi-shot order
//!   the re-drive *would* enter the gate and could consume a `max_retries` slot
//!   for an entry the operator already placed once. That is an OPEN QUESTION
//!   (TODO.md, sub-plan-5 #4): the safe long-term answer is a `restoring` flag
//!   threaded into `record_placement`; not built here. Today the re-drive of a
//!   multi-shot order can burn a slot — acceptable until multi-shot resting
//!   orders are demo-exercised. Single-shot (the motivating H&S/M-W case) is
//!   unaffected.

use chrono::{DateTime, Utc};
use trade_control_core::blackout_recreate::{RestorePlan, restore_plan};
use trade_control_core::incoming::{self, IncomingError};
use trade_control_core::intent::Resolved;
use trade_control_core::state::{CancelledOrder, SpreadBlackoutRecord};
use worker::{Env, console_error, console_log};

use super::sweep::{BrokerHandle, acquire_broker_for_account};
use crate::state::KvStateStore;

/// Re-drive (or drop) every cancelled resting order on a record. Called by the
/// recovery watcher at both clear points (recovery + backstop), BEFORE the
/// record is cleared. Per-order errors are logged and skipped — one bad
/// re-drive must never block the others or the clear.
pub async fn restore_cancelled_orders(
    env: &Env,
    store: &KvStateStore,
    record: &SpreadBlackoutRecord,
    now: DateTime<Utc>,
) {
    if record.cancelled_orders.is_empty() {
        return;
    }
    for cancelled in &record.cancelled_orders {
        if let Err(err) = restore_one_order(env, store, record, cancelled, now).await {
            console_error!(
                "blackout restore[{}]: order {} re-drive error: {err}",
                record.trade_id,
                cancelled.order_id,
            );
        }
    }
}

/// Re-drive or drop one cancelled order. Returns an error string only for
/// genuinely unexpected failures (broker/key acquisition); every *expected*
/// drop path (expired body, stale limit, overrun-skip stop) returns `Ok(())`
/// after logging, so the watcher treats them as handled.
async fn restore_one_order(
    env: &Env,
    store: &KvStateStore,
    record: &SpreadBlackoutRecord,
    cancelled: &CancelledOrder,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let tid = &record.trade_id;

    // 1. Reconstruct an authentic Verified from the stored signed body.
    let key = crate::signing_key(env).ok_or("no signing key")?;
    let verified = match incoming::parse_and_verify(&cancelled.signed_intent, &key, now) {
        Ok(v) => v,
        Err(IncomingError::Expired) | Err(IncomingError::StaleShellTime) => {
            console_log!(
                "blackout restore[{tid}]: stored intent expired, dropped order {} \
                 (window closed during blackout)",
                cancelled.order_id,
            );
            cleanup_body(store, &cancelled.order_id).await;
            return Ok(());
        }
        Err(e) => return Err(format!("re-verify stored intent: {e}")),
    };

    // 2. Fill-side pre-check using the pure restore_plan + a fresh quote.
    let broker = acquire_broker_for_account(env, record.account.as_deref())
        .await
        .ok_or("broker acquisition failed")?;
    // pip_size: prefer the baked intent value, else the record's (apply-time)
    // value, else the forex default — only needed to resolve absolute prices.
    let pip = verified
        .intent
        .pip_size
        .filter(|p| *p > 0.0 && p.is_finite())
        .or(Some(record.pip_size).filter(|p| *p > 0.0 && p.is_finite()))
        .unwrap_or(crate::DEFAULT_PIP_SIZE);
    let resolved = Resolved::from_intent(&verified.intent, &verified.shell, pip)
        .map_err(|e| format!("resolve: {e}"))?;
    let quote = get_quote(&broker, &resolved.instrument)
        .await
        .map_err(|e| format!("quote: {e:?}"))?;
    let on_too_close = resolved.on_too_close.as_ref().map(|o| o.action);

    let plan = restore_plan(
        &resolved.entry,
        resolved.direction,
        resolved.stop_loss,
        resolved.take_profit,
        quote.bid,
        quote.ask,
        on_too_close,
    );
    match plan {
        RestorePlan::DropStopOverrunSkip => {
            console_log!(
                "blackout restore[{tid}]: stop overrun, on_too_close=skip, dropped order {} \
                 (bid={} ask={})",
                cancelled.order_id,
                quote.bid,
                quote.ask,
            );
            cleanup_body(store, &cancelled.order_id).await;
            return Ok(());
        }
        RestorePlan::DropStaleLimit => {
            console_log!(
                "blackout restore[{tid}]: limit stale (bid/ask wrong side), dropped order {} \
                 — trade left looking for entry (bid={} ask={})",
                cancelled.order_id,
                quote.bid,
                quote.ask,
            );
            cleanup_body(store, &cancelled.order_id).await;
            return Ok(());
        }
        RestorePlan::DropUnexpectedMarket => {
            console_log!(
                "blackout restore[{tid}]: unexpected resting market order {}, dropped \
                 (market entries never rest)",
                cancelled.order_id,
            );
            cleanup_body(store, &cancelled.order_id).await;
            return Ok(());
        }
        RestorePlan::Redrive => {}
    }

    // 3. Re-drive through run_enter. SAME intended entry — we do NOT call
    //    mark_seen (the original id is already seen; the cron is off the HTTP
    //    is_seen path entirely) and we pass the signed body so a successfully
    //    re-placed order re-stores its own order:{order_id} row (it can be
    //    blackout-cancelled again later). The on_too_close fallback fires
    //    naturally on the broker's #19-10 if the trigger was overrun beyond the
    //    band but on_too_close != skip.
    let result = redrive(
        &broker,
        store,
        &verified,
        env,
        now,
        &cancelled.signed_intent,
    )
    .await;
    console_log!(
        "blackout restore[{tid}]: re-drive order {} → {}",
        cancelled.order_id,
        result.describe(),
    );
    // The old order id is consumed: its body row is no longer the live order.
    // (A fresh order, if placed, stored its own new body inside run_enter.)
    cleanup_body(store, &cancelled.order_id).await;
    Ok(())
}

/// Best-effort delete of the stored order body once it's been handled (placed,
/// dropped, or expired). Logged, never fatal — the TTL is the backstop.
async fn cleanup_body(store: &KvStateStore, order_id: &str) {
    if let Err(err) = store.delete_order_body(order_id).await {
        console_error!("blackout restore: delete_order_body({order_id}) failed: {err}");
    }
}

/// Re-drive via `run_enter` against the concrete inner broker. `BrokerHandle`
/// type-erases the broker, so we match here to hand `run_enter` a single
/// `impl Broker` (it's generic over `B: Broker`, which an enum can't satisfy).
async fn redrive(
    broker: &BrokerHandle,
    store: &KvStateStore,
    verified: &incoming::Verified,
    env: &Env,
    now: DateTime<Utc>,
    raw_body: &str,
) -> crate::ActionResult {
    match broker {
        BrokerHandle::Oanda(b) => {
            crate::run_enter(b, store, verified, env, now, Some(raw_body)).await
        }
        BrokerHandle::TradeNation(b) => {
            crate::run_enter(b, store, verified, env, now, Some(raw_body)).await
        }
    }
}

async fn get_quote(
    broker: &BrokerHandle,
    instrument: &str,
) -> Result<trade_control_core::broker::Quote, trade_control_core::broker::LookupError> {
    match broker {
        BrokerHandle::Oanda(b) => {
            use trade_control_core::broker::Broker;
            b.get_quote(instrument).await
        }
        BrokerHandle::TradeNation(b) => {
            use trade_control_core::broker::Broker;
            b.get_quote(instrument).await
        }
    }
}
