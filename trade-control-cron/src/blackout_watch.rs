//! The every-tick spread-blackout driver (PR 2 — live cron cutover to the shared
//! resting-order lifecycle).
//!
//! Per affected account it runs the SHARED
//! [`core::pending_order_lifecycle`](trade_control_core::pending_lifecycle::pending_order_lifecycle)
//! — the SAME function the offline replay drives, so the resting-order cancel +
//! restore DECISION (System 3) runs identically live and in replay. It is called
//! with [`ClearPolicy::LeaveForCaller`](trade_control_core::pending_lifecycle::ClearPolicy),
//! so the shared fn restores System 3 but LEAVES the record; this watcher then
//! restores **System 2** (widened open-position stops → remembered originals) and
//! issues the single record `clear`. The coexistence contract — restore both
//! System 2 and System 3, then clear once — is preserved on ONE unified OFF
//! trigger (the shared fn's `off_now`: block-lifted OR live-spread-recovered).
//!
//! ## What moved to the shared fn (`core::pending_lifecycle`)
//!
//! - The ON cancel (now the pure baked clock `is_spread_hour`, was a live-quote
//!   `spread > 5×median` sample — the PR-2 trigger delta).
//! - The OFF recovery decision + the safety force-restore ceiling + their pure
//!   predicates and tests.
//! - The System-3 re-drive via `run_enter` (RAIL 7, `restore = true`).
//!
//! ## What stays live-specific here
//!
//! - **System 2** ([`restore_remembered_stops`]) — `amend_stop` an open
//!   position's widened SL back to its remembered original. Not part of the shared
//!   System-3 fn (see the `LeaveForCaller` note in [`ClearPolicy`]).
//! - The per-account fan-out + the single record `clear`.
//!
//! # Runtime-agnostic via the [`CronEnv`] seam
//!
//! Both the wasm worker and the native scheduler drive this via [`CronEnv`]; the
//! caller opens the [`StateStore`] and passes it in.

use chrono::{DateTime, Utc};
use trade_control_core::broker::{AmendError, Broker};
use trade_control_core::order_control::{
    CurrentStop, OriginalStop, RestoreDecision, restore_decision,
};
use trade_control_core::pending_lifecycle::ClearPolicy;
use trade_control_core::state::{HeldTradeRecord, StateStore};

use crate::broker_handle::BrokerHandle;
use crate::seam::CronEnv;
use crate::spread_lifecycle::run_spread_lifecycle_for_account;

/// The single every-tick spread-blackout driver (PR 2).
///
/// For EACH affected account it runs the SHARED
/// `core::pending_order_lifecycle` (the SAME fn the offline replay calls) with
/// [`ClearPolicy::LeaveForCaller`]: that CANCELS resting orders entering a
/// per-instrument spread hour (baked clock, System 3 ON) and RESTORES System 3
/// for records whose trough has lifted — but LEAVES each record. Then, for every
/// record the shared fn reported restored, this watcher restores **System 2**
/// (widened open-position stops → remembered originals) and issues the single
/// record `clear`. So the coexistence contract holds ("restore System 2 + System
/// 3, clear once") on ONE unified OFF trigger (the shared fn's `off_now`).
///
/// Why the CANCEL lives here (not in the NY-close apply): the shared fn does
/// cancel + recover in one call, so driving it from ONE loop makes each happen
/// exactly once per tick with no double-run. The watch loop is every-tick, so a
/// resting order entering ANY instrument's baked spread hour is cancelled
/// promptly — not just at the single global NY-close edge the old apply-side
/// cancel keyed on.
///
/// Per-account / per-record errors are logged and skipped — one bad row must
/// never abort the loop (same discipline as the order sweep).
pub async fn watch_recovery<S, C>(store: &S, cron: &C, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    for account in affected_accounts(store).await {
        watch_account(store, cron, account.as_deref(), now).await;
    }
}

/// The distinct accounts to drive this tick: the union of accounts on existing
/// blackout records (their System 2/3 may need restoring) and on tracked
/// `EntryAttempt` rows (their resting orders may need CANCELLING before any
/// record exists). Mirrors the old cancel path's account discovery, plus the
/// record side the watcher already walked.
async fn affected_accounts<S: StateStore>(store: &S) -> Vec<Option<String>> {
    let mut accounts: Vec<Option<String>> = Vec::new();
    let mut push = |acc: &Option<String>| {
        if !accounts.contains(acc) {
            accounts.push(acc.clone());
        }
    };
    match store.list_all_entry_attempts().await {
        Ok(v) => v.iter().for_each(|a| push(&a.account)),
        Err(err) => tracing::error!("blackout watch: list_all_entry_attempts: {err}"),
    }
    match store.list_all_held_trade_records().await {
        Ok(v) => v.iter().for_each(|r| push(&r.account)),
        Err(err) => tracing::error!("blackout watch: list records failed: {err}"),
    }
    accounts
}

/// Drive one account: shared cancel+restore (System 3, leaving records), then
/// per restored record restore System 2 + clear.
async fn watch_account<S, C>(store: &S, cron: &C, account: Option<&str>, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    // System 1/3 cancel + restore via the shared fn — leaves the record for us.
    let report =
        run_spread_lifecycle_for_account(store, cron, account, now, ClearPolicy::LeaveForCaller)
            .await;

    // For every record the shared fn reported restored (its OFF trigger fired,
    // System 3 restored), restore System 2 then clear — SAME OFF decision, so
    // both halves restore together and the record clears once (coexistence).
    for (trade_id, _reason) in &report.restored {
        if let Err(err) = finish_system2_and_clear(store, cron, trade_id).await {
            tracing::error!("blackout watch[{trade_id}]: System-2 restore/clear: {err}");
        }
    }

    // A failed cancel means the DB and the broker disagreed about a resting order
    // on the live money path — surface it at account scope, not just in the
    // per-order line inside the shared fn. `Vanished` entries have already been
    // pruned from their record (nothing to restore); the others retry next tick.
    if !report.cancel_failed.is_empty() {
        tracing::warn!(
            "blackout watch[{}]: {} resting order(s) could not be cancelled: {}",
            account.unwrap_or("<global>"),
            report.cancel_failed.len(),
            report
                .cancel_failed
                .iter()
                .map(|(id, outcome)| format!("{id}={outcome:?}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
}

/// The System-2 half + the single clear for one restored record. Re-reads the
/// record (the shared fn left it under `LeaveForCaller`), restores its widened
/// stops verbatim, then deletes the record. A record already gone (raced clear)
/// is benign.
async fn finish_system2_and_clear<S, C>(store: &S, cron: &C, trade_id: &str) -> Result<(), String>
where
    S: StateStore,
    C: CronEnv,
{
    let record = match store.get_held_trade_record(trade_id).await {
        Ok(Some(r)) => r,
        // Already cleared (raced) — nothing to do.
        Ok(None) => return Ok(()),
        Err(err) => return Err(format!("get_record: {err}")),
    };
    // System 2: widened stops → remembered originals (independent of System 3's
    // cancelled_orders list; a trade may carry both).
    restore_remembered_stops(cron, &record).await;
    clear(store, &record, "recovery").await?;
    tracing::info!("blackout watch[{trade_id}]: System 2 restored + record cleared",);
    Ok(())
}

/// Restore every remembered widened stop to its **remembered original**,
/// then return. The hard rule: restore from `remembered.original_stop`
/// VERBATIM, never `current − widen` — a partial widen / missed tick /
/// double-fire all stay correct because the remembered original is
/// idempotent (restoring twice lands on the same number). Per-id errors are
/// logged and skipped so the clear still proceeds; a closed position yields
/// `AmendError::NotFound` and is treated as benign (nothing to restore).
/// System 2 only ever moves a stop — it never closes or tightens.
///
/// **Guarded against a second writer.** Verbatim restore is only safe while
/// System 2 is the sole owner of the stop, and it isn't: `breakeven_watch`
/// (System F) amends the same handle on the same cadence. Restoring blindly
/// would revert a locked-in break-even to a real-money level. So each stop is
/// read back from the broker and put through the pure
/// [`restore_decision`](trade_control_core::order_control::restore_decision) —
/// a stop that has moved *tighter* than the remembered original belongs to
/// whoever moved it, and is left alone. See `core::order_control::restore`.
async fn restore_remembered_stops<C: CronEnv>(cron: &C, record: &HeldTradeRecord) {
    if record.original_stops.is_empty() {
        return;
    }
    let Some(broker) = cron.acquire_broker(record.account.as_deref()).await else {
        tracing::error!(
            "blackout restore[{}]: broker acquisition failed — {} stop(s) left widened until \
             the operator restores them (backstop TTL is the final net)",
            record.trade_id,
            record.original_stops.len(),
        );
        return;
    };
    let account = record.account.as_deref().unwrap_or("");

    // Read the stops back as they ACTUALLY are at the broker. The whole failure
    // mode is another system having moved one, so a believed level is no use.
    // A lookup failure is fail-CLOSED: with no way to tell a widened stop from a
    // break-even one, leaving it widened risks a wider loss, while restoring
    // blindly risks giving back a locked-in win. We keep the protection.
    let positions = match &broker {
        BrokerHandle::Oanda(b) => b.list_open_positions(account).await,
        BrokerHandle::TradeNation(b) => b.list_open_positions(account).await,
    };
    let positions = match positions {
        Ok(p) => p,
        Err(err) => {
            tracing::error!(
                "blackout restore[{}]: list_open_positions FAILED ({err}) — cannot tell a widened \
                 stop from a break-even one, so {} stop(s) left WIDENED (fail-closed; retried next \
                 tick, backstop TTL is the final net)",
                record.trade_id,
                record.original_stops.len(),
            );
            return;
        }
    };

    for remembered in &record.original_stops {
        let id = &remembered.position_or_order_id;
        // Match on either handle: `order_id` is the documented TN amend key that
        // the widen stored, but OANDA callers may hold the trade id.
        let Some(position) = positions
            .iter()
            .find(|p| p.order_id == *id || p.position_id == *id)
        else {
            tracing::info!(
                "blackout restore[{}]: id={id} not in open positions (closed during window) — \
                 benign, nothing to restore",
                record.trade_id,
            );
            continue;
        };
        let Some(current_stop) = position.stop_loss else {
            tracing::warn!(
                "blackout restore[{}]: id={id} has NO stop attached — not restoring (a stop we \
                 widened should still exist; re-attaching one here could contradict whatever \
                 removed it)",
                record.trade_id,
            );
            continue;
        };

        match restore_decision(
            position.direction,
            CurrentStop(current_stop),
            OriginalStop(remembered.original_stop),
        ) {
            RestoreDecision::Skip(reason) => {
                tracing::info!(
                    "blackout restore[{}]: id={id} SKIPPED ({reason:?}) — current {current_stop} \
                     vs remembered original {} (dir={:?}); leaving the tighter stop in place",
                    record.trade_id,
                    remembered.original_stop,
                    position.direction,
                );
                continue;
            }
            RestoreDecision::RestoreTo(level) => {
                let result = match &broker {
                    BrokerHandle::Oanda(b) => b.amend_stop(account, id, level).await,
                    BrokerHandle::TradeNation(b) => b.amend_stop(account, id, level).await,
                };
                match result {
                    Ok(()) => tracing::info!(
                        "blackout restore[{}]: amend_stop ok id={id} {current_stop} -> original \
                         {level} (verbatim, no recompute)",
                        record.trade_id,
                    ),
                    Err(AmendError::NotFound) => tracing::info!(
                        "blackout restore[{}]: id={id} gone (closed during window) — benign, \
                         nothing to restore",
                        record.trade_id,
                    ),
                    Err(err) => tracing::error!(
                        "blackout restore[{}]: amend_stop id={id} -> {level} FAILED ({err}) — stop \
                         left WIDENED, operator must restore manually",
                        record.trade_id,
                    ),
                }
            }
        }
    }
}

async fn clear<S: StateStore>(
    store: &S,
    record: &HeldTradeRecord,
    reason: &str,
) -> Result<(), String> {
    store
        .clear_held_trade_record(&record.trade_id)
        .await
        .map_err(|e| format!("{reason} clear: {e}"))
}

// The OFF-side recovery/backstop DECISION (spread-recovered, block-lift, the 12h
// safety ceiling) moved to `core::pending_lifecycle` (PR 2) — the SAME fn the
// replay drives — so the cron no longer owns those predicates. `spread_recovered`
// / `backstop_due` / `spread_in_pips` and their tests now live in `core`. This
// module keeps only the LIVE-specific System-2 restore (`restore_remembered_stops`)
// + the record `clear`.

// The OFF-side recovery/backstop DECISION tests (spread_recovered / backstop_due
// / spread_in_pips / the `!applied` short-circuit) moved to
// `core::pending_lifecycle::tests` with the decision itself (PR 2), so the live
// cron and the replay assert the SAME behaviour once. What remains live-specific
// here (System-2 widen restore, the record clear, the account fan-out) is covered
// by the shared-fn tests + the worker build.
