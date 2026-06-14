//! Cron 1 — NY-close-edge handler. Fires from the daily candidate crons
//! (`5 21 * * *` / `5 22 * * *`). When `is_ny_close_edge(now)`, opens the
//! global spread-blackout window marker, then — **System 2** — widens every
//! open position's stop-loss away from price so the post-NY-close spread
//! blowout can't clip it, remembering each original stop on a per-trade
//! `SpreadBlackoutRecord` for the recovery watcher to restore.
//!
//! Per-row discipline (same as the order sweep): one position's
//! `list_open_positions` / `amend_stop` failure logs and continues — it must
//! never abort the widen of the other open trades.
//!
//! **PRECONDITION (hard gate):** `amend_stop` on an OPEN position via
//! TradeNation's `AmendCloseOrder` is UNVERIFIED (zero upstream callers). The
//! widen must be demo-confirmed on `reversals` (open a position, amend the SL,
//! read it back, confirm SL moved + TP unchanged) before this is trusted live.
//! See TODO.md + CHANGELOG. The code is structured so every intended amend is
//! logged prominently first, so a dry-run/demo can confirm the read-back.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use trade_control_core::broker::{AmendError, Broker, OpenPosition};
use trade_control_core::ny_clock::is_ny_close_edge;
use trade_control_core::state::{EntryAttempt, RememberedStop, SpreadBlackoutRecord, StateStore};
use worker::Env;

use super::blackout_widen::{clamp_widen, widened_stop};
use super::constants::BLACKOUT_BACKSTOP_SECONDS;
use super::sweep::{BrokerHandle, acquire_broker_for_account, open_store};
use crate::state::KvStateStore;

/// Open the global spread-blackout window marker iff `now` is the
/// NY-close edge, then widen open stops (System 2). The two daily crons
/// fire at both DST candidate hours (21:05 EDT, 22:05 EST);
/// `is_ny_close_edge` decides which one is the real edge this season and
/// no-ops the other.
pub async fn apply_if_ny_close_edge(env: &Env, now: DateTime<Utc>) {
    if !is_ny_close_edge(now) {
        rlog!("blackout: cron fired but not NY-close edge ({now}); no-op");
        return;
    }
    let Some(store) = open_store(env) else {
        return;
    };
    // The window TTL keys off the same backstop the recovery watcher
    // uses, so the marker and the per-record backstop can never drift.
    let ttl = BLACKOUT_BACKSTOP_SECONDS;
    match store.set_spread_blackout_window(now, ttl).await {
        Ok(()) => rlog!("blackout: window opened at {now} (ttl {ttl}s)"),
        Err(err) => rlog_err!("blackout: failed to open window: {err}"),
    }

    // System 2 — widen open stops away from price.
    widen_open_stops(env, &store, now).await;
    // System 3 — cancel resting entry orders on elevated-spread instruments
    // and store their signed intent for the recovery watcher to re-drive.
    // Runs on the SAME affected-account set as the widen; the two share one
    // `SpreadBlackoutRecord` per trade (widened stops in `original_stops`,
    // cancelled orders in `cancelled_orders`) and the watcher restores both.
    super::blackout_cancel::cancel_resting_orders(env, now).await;
}

/// List open positions per affected account, join each to its originating
/// `EntryAttempt` (for `trade_id` + baked `pip_size`), and widen the stop.
///
/// **Affected accounts** are sourced from the `EntryAttempt` rows
/// themselves — the set of `account`s that have a tracked entry *is* the set
/// with positions we can join + pip-resolve, so this is self-scoping. (The
/// confirmed broker today is TN `reversals`; if more accounts land this still
/// covers them. A position with no `EntryAttempt` at all can't be pip-resolved
/// and is skipped — see the join open question.)
async fn widen_open_stops(env: &Env, store: &KvStateStore, now: DateTime<Utc>) {
    let attempts = match store.list_all_entry_attempts().await {
        Ok(v) => v,
        Err(err) => {
            rlog_err!("blackout widen: list_all_entry_attempts: {err}");
            return;
        }
    };
    // Distinct affected accounts (preserves insertion order, dedups).
    let mut accounts: Vec<Option<String>> = Vec::new();
    for a in &attempts {
        if !accounts.contains(&a.account) {
            accounts.push(a.account.clone());
        }
    }
    rlog!(
        "blackout widen: {} attempt(s), {} affected account(s)",
        attempts.len(),
        accounts.len(),
    );
    for account in accounts {
        widen_account(env, store, &attempts, account.as_deref(), now).await;
    }
}

/// Widen every open position on one account. Logs + skips per position.
async fn widen_account(
    env: &Env,
    store: &KvStateStore,
    attempts: &[EntryAttempt],
    account: Option<&str>,
    now: DateTime<Utc>,
) {
    let Some(broker) = acquire_broker_for_account(env, account).await else {
        rlog_err!(
            "blackout widen[{}]: broker acquisition failed; skipping account",
            account.unwrap_or("<global>"),
        );
        return;
    };
    let account_id = account.unwrap_or("");
    let positions = match list_positions(&broker, account_id).await {
        Ok(p) => p,
        Err(err) => {
            rlog_err!(
                "blackout widen[{}]: list_open_positions: {err}",
                account.unwrap_or("<global>"),
            );
            return;
        }
    };
    // Sample each instrument's spread once and reuse across that
    // instrument's positions (the cron acts on a handful of trades).
    let mut spread_cache: HashMap<String, Option<f64>> = HashMap::new();
    for position in positions {
        widen_one(
            store,
            &broker,
            account,
            attempts,
            &position,
            &mut spread_cache,
            now,
        )
        .await;
    }
}

/// Widen a single open position's stop. Order of operations is crash-safe:
/// record the original FIRST, then amend the broker stop. A crash after the
/// record but before the amend just means restore is a harmless no-op (amend
/// back to a stop already at the original); the reverse (amend then crash)
/// would strand a widened stop with no remembered original.
#[allow(clippy::too_many_arguments)]
async fn widen_one(
    store: &KvStateStore,
    broker: &BrokerHandle,
    account: Option<&str>,
    attempts: &[EntryAttempt],
    position: &OpenPosition,
    spread_cache: &mut HashMap<String, Option<f64>>,
    now: DateTime<Utc>,
) {
    let Some(original_sl) = position.stop_loss else {
        // Nothing to widen — a position with no attached stop is left alone
        // (System 2 only ever moves an existing stop, never adds one).
        return;
    };

    // Join → originating attempt (for trade_id + baked pip_size).
    let Some(attempt) = join_position_to_attempt(position, account, attempts) else {
        rlog!(
            "blackout widen[{}]: position {} ({}) has no joinable EntryAttempt; skip (can't \
             resolve trade_id/pip)",
            account.unwrap_or("<global>"),
            position.position_id,
            position.instrument,
        );
        return;
    };
    let trade_id = attempt.trade_id.clone();

    // Idempotency guard — only widen if no `applied` record exists for this
    // trade. A re-fire (CF double-deliver / mid-window restart) must not
    // double-widen or re-capture the (already-widened) SL as "original".
    match store.get_spread_blackout_record(&trade_id).await {
        Ok(Some(rec)) if rec.applied => {
            rlog!(
                "blackout widen[{}]: trade {trade_id} already applied; skip",
                account.unwrap_or("<global>"),
            );
            return;
        }
        Ok(_) => {}
        Err(err) => {
            rlog_err!(
                "blackout widen[{}]: get_record({trade_id}): {err}",
                account.unwrap_or("<global>")
            );
            return;
        }
    }

    // Pip from the joined attempt — never widen with a wrong/absent pip.
    let Some(pip_size) = attempt.pip_size.filter(|p| *p > 0.0 && p.is_finite()) else {
        rlog!(
            "blackout widen[{}]: trade {trade_id} has no usable pip_size; skip (won't widen with \
             a wrong pip)",
            account.unwrap_or("<global>"),
        );
        return;
    };

    // Live spread → pips → clamped widen → new SL.
    let Some(spread_abs) =
        sample_instrument_spread(broker, &position.instrument, spread_cache).await
    else {
        rlog_err!(
            "blackout widen[{}]: get_quote({}) failed; skip trade {trade_id}",
            account.unwrap_or("<global>"),
            position.instrument,
        );
        return;
    };
    let spread_pips = spread_abs / pip_size;
    let widen_pips = clamp_widen(spread_pips);
    let new_sl = widened_stop(position.direction, original_sl, widen_pips, pip_size);

    // RECORD FIRST (crash-safe), then amend. `order_id` is the documented
    // TN amend key — store the same id so restore amends the same handle.
    let record = SpreadBlackoutRecord {
        trade_id: trade_id.clone(),
        instrument: position.instrument.clone(),
        account: account.map(|s| s.to_string()),
        applied: true,
        opened_at: now,
        expires_at: now + Duration::seconds(BLACKOUT_BACKSTOP_SECONDS as i64),
        pip_size,
        original_stops: vec![RememberedStop {
            position_or_order_id: position.order_id.clone(),
            original_stop: original_sl,
        }],
        cancelled_orders: Vec::new(),
    };
    if let Err(err) = store
        .upsert_spread_blackout_record(&record, BLACKOUT_BACKSTOP_SECONDS)
        .await
    {
        rlog_err!(
            "blackout widen[{}]: upsert_record({trade_id}) FAILED ({err}); NOT amending (no \
             remembered original ⇒ no widen)",
            account.unwrap_or("<global>"),
        );
        return;
    }

    // PRECONDITION-guarded amend: log the intended amend prominently so a
    // demo run can read it back and confirm `AmendCloseOrder`-on-open-position
    // actually moved the SL (and left TP) before this is trusted live.
    rlog!(
        "blackout widen[{}]: INTENT amend_stop trade={trade_id} id={} instrument={} dir={:?} \
         original_sl={original_sl} spread_pips={spread_pips:.1} widen_pips={widen_pips} \
         new_sl={new_sl} (DEMO-CONFIRM AmendCloseOrder-on-open-position before trusting live)",
        account.unwrap_or("<global>"),
        position.order_id,
        position.instrument,
        position.direction,
    );
    match amend(broker, account.unwrap_or(""), &position.order_id, new_sl).await {
        Ok(()) => rlog!(
            "blackout widen[{}]: amend_stop ok trade={trade_id} id={} -> {new_sl}",
            account.unwrap_or("<global>"),
            position.order_id,
        ),
        Err(AmendError::NotFound) => rlog!(
            "blackout widen[{}]: amend_stop id={} not found (position closed?) trade={trade_id} — \
             benign; record stays for restore-as-noop / backstop clear",
            account.unwrap_or("<global>"),
            position.order_id,
        ),
        Err(err) => rlog_err!(
            "blackout widen[{}]: amend_stop trade={trade_id} id={} -> {new_sl} FAILED ({err}); \
             record persisted so recovery still restores to original",
            account.unwrap_or("<global>"),
            position.order_id,
        ),
    }
}

/// Sample the instrument's live spread once, memoised across positions on
/// the same instrument. `None` caches a failed quote so we don't re-hit a
/// broken endpoint for every position on that instrument this tick.
async fn sample_instrument_spread(
    broker: &BrokerHandle,
    instrument: &str,
    cache: &mut HashMap<String, Option<f64>>,
) -> Option<f64> {
    if let Some(v) = cache.get(instrument) {
        return *v;
    }
    let quote = match broker {
        BrokerHandle::Oanda(b) => b.get_quote(instrument).await,
        BrokerHandle::TradeNation(b) => b.get_quote(instrument).await,
    };
    let spread = quote.ok().map(|q| q.spread());
    cache.insert(instrument.to_string(), spread);
    spread
}

async fn list_positions(
    broker: &BrokerHandle,
    account_id: &str,
) -> Result<Vec<OpenPosition>, String> {
    let res = match broker {
        BrokerHandle::Oanda(b) => b.list_open_positions(account_id).await,
        BrokerHandle::TradeNation(b) => b.list_open_positions(account_id).await,
    };
    res.map_err(|e| e.to_string())
}

async fn amend(
    broker: &BrokerHandle,
    account_id: &str,
    id: &str,
    new_stop: f64,
) -> Result<(), AmendError> {
    match broker {
        BrokerHandle::Oanda(b) => b.amend_stop(account_id, id, new_stop).await,
        BrokerHandle::TradeNation(b) => b.amend_stop(account_id, id, new_stop).await,
    }
}

/// Join an open position to the `EntryAttempt` that placed it — the source
/// of `trade_id` + baked `pip_size`. Pure & unit-testable.
///
/// Principled match: `position.position_id == attempt.broker_trade_id`
/// (TN PositionID / OANDA trade id, snapshotted on the attempt once the fill
/// is observed). Fallback: `instrument + direction + account` when no
/// `broker_trade_id` has been snapshotted yet (a just-opened position
/// blacked out before the snapshot landed).
///
/// OPEN QUESTION (do NOT resolve here): the fallback aliases if two
/// concurrent trades run on the same pair/direction/account — rare for the
/// affected thin crosses (one setup per instrument is the common case), but
/// real. A fully unambiguous join would need a synthetic per-`position_id`
/// record when nothing correlates. Noted in TODO.md.
fn join_position_to_attempt<'a>(
    position: &OpenPosition,
    account: Option<&str>,
    attempts: &'a [EntryAttempt],
) -> Option<&'a EntryAttempt> {
    // 1. Exact: snapshotted broker_trade_id == position_id.
    if let Some(hit) = attempts
        .iter()
        .find(|a| a.broker_trade_id.as_deref() == Some(position.position_id.as_str()))
    {
        return Some(hit);
    }
    // 2. Coarse fallback: instrument + direction + account.
    attempts.iter().find(|a| {
        a.instrument == position.instrument
            && a.direction == position.direction
            && a.account.as_deref() == account
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_core::intent::Direction;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    fn attempt(
        trade_id: &str,
        instrument: &str,
        direction: Direction,
        account: Option<&str>,
        broker_trade_id: Option<&str>,
        pip_size: Option<f64>,
    ) -> EntryAttempt {
        EntryAttempt {
            trade_id: trade_id.into(),
            account: account.map(|s| s.into()),
            instrument: instrument.into(),
            attempt_no: 1,
            broker_order_id: "ord-1".into(),
            broker_trade_id: broker_trade_id.map(|s| s.into()),
            direction,
            placed_at: ts("2026-03-12T20:00:00Z"),
            shell_time: ts("2026-03-12T20:00:00Z"),
            expires_at: ts("2026-03-13T00:00:00Z"),
            stop_loss_price: Some(1.8000),
            cancel_at: None,
            pip_size,
        }
    }

    fn position(instrument: &str, direction: Direction, position_id: &str) -> OpenPosition {
        OpenPosition {
            instrument: instrument.into(),
            direction,
            stop_loss: Some(1.8000),
            take_profit: None,
            position_id: position_id.into(),
            order_id: "ord-1".into(),
            stake: 1.0,
        }
    }

    #[test]
    fn join_matches_on_broker_trade_id_first() {
        let attempts = vec![
            // Coarse-matching decoy that would alias on the fallback.
            attempt(
                "decoy",
                "EUR_NZD",
                Direction::Short,
                Some("reversals"),
                None,
                Some(0.0001),
            ),
            attempt(
                "real",
                "EUR_NZD",
                Direction::Short,
                Some("reversals"),
                Some("POS-99"),
                Some(0.0001),
            ),
        ];
        let pos = position("EUR_NZD", Direction::Short, "POS-99");
        let hit = join_position_to_attempt(&pos, Some("reversals"), &attempts).unwrap();
        assert_eq!(hit.trade_id, "real");
    }

    #[test]
    fn join_falls_back_to_instrument_direction_account() {
        let attempts = vec![attempt(
            "t1",
            "AUD_NZD",
            Direction::Long,
            Some("reversals"),
            None, // no snapshotted broker_trade_id yet
            Some(0.0001),
        )];
        // position_id won't match any broker_trade_id; fallback kicks in.
        let pos = position("AUD_NZD", Direction::Long, "POS-NEW");
        let hit = join_position_to_attempt(&pos, Some("reversals"), &attempts).unwrap();
        assert_eq!(hit.trade_id, "t1");
    }

    #[test]
    fn join_misses_when_nothing_correlates() {
        let attempts = vec![attempt(
            "t1",
            "EUR_USD",
            Direction::Long,
            Some("reversals"),
            None,
            Some(0.0001),
        )];
        let pos = position("EUR_NZD", Direction::Short, "POS-X");
        assert!(join_position_to_attempt(&pos, Some("reversals"), &attempts).is_none());
    }

    #[test]
    fn join_fallback_respects_account_scope() {
        let attempts = vec![attempt(
            "t1",
            "EUR_NZD",
            Direction::Short,
            Some("other-account"),
            None,
            Some(0.0001),
        )];
        let pos = position("EUR_NZD", Direction::Short, "POS-X");
        // Same instrument+direction but different account → no match.
        assert!(join_position_to_attempt(&pos, Some("reversals"), &attempts).is_none());
    }
}
