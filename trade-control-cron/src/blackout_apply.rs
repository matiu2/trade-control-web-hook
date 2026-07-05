//! Cron 1 — NY-close-edge handler. Fired from the 15-min cron arm gated by the
//! caller on `is_ny_close_edge`. When `is_ny_close_edge(now)`, opens the global
//! spread-blackout window marker, then — **System 2** — widens every open
//! position's stop-loss away from price so the post-NY-close spread blowout
//! can't clip it, remembering each original stop on a per-trade
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
//!
//! # Runtime-agnostic via the [`CronEnv`] seam
//!
//! Moved into `trade-control-cron` so both the wasm Cloudflare worker and the
//! native VM scheduler run the *same* NY-close apply. The `&Env`-hidden broker
//! acquisition travels through the [`CronEnv`] seam; the caller opens the
//! [`StateStore`] and passes it in. The fn self-gates on `is_ny_close_edge` —
//! the caller's once-per-hour `now.minute() < 15` guard (wasm) or upkeep tick
//! (native) only avoids redundant broker calls.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use trade_control_core::blackout_widen::{clamp_widen, spread_hour_widen_size, widened_stop};
use trade_control_core::broker::{AmendError, Broker, OpenPosition};
use trade_control_core::ny_clock::is_ny_close_edge;
use trade_control_core::spread_blackout::spread_hour_widen_pips;
use trade_control_core::state::{EntryAttempt, RememberedStop, SpreadBlackoutRecord, StateStore};

use crate::broker_handle::BrokerHandle;
use crate::constants::BLACKOUT_BACKSTOP_SECONDS;
use crate::seam::CronEnv;

/// Open the global spread-blackout window marker + cancel resting entry
/// orders iff `now` is the NY-close edge. **System 1 (entry-reject window)
/// and System 3 (cancel resting orders)** — both keyed to the single global
/// NY-close concept. The caller fires this on every candidate tick;
/// `is_ny_close_edge` decides which is the real edge this season and no-ops
/// the rest.
///
/// **System 2 (widen open stops) is NOT here** — it moved to
/// [`widen_open_stops_for_spread_hours`], which is per-instrument (each
/// instrument's own learned spread hours from the baked sampler data) and
/// fires on every tick, not just at the NY-close edge. See that fn's docs
/// for why the split happened (the 2026-07-05 spread-hour analysis: the
/// widen must fire at *each instrument's* elevated hours — Gold overnight,
/// EUR/USD 21:00, indices at their own — not one global NY-close hour).
pub async fn apply_if_ny_close_edge<S, C>(store: &S, cron: &C, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    if !is_ny_close_edge(now) {
        tracing::info!("blackout: cron fired but not NY-close edge ({now}); no-op");
        return;
    }
    // The window TTL keys off the same backstop the recovery watcher
    // uses, so the marker and the per-record backstop can never drift.
    let ttl = BLACKOUT_BACKSTOP_SECONDS;
    match store.set_spread_blackout_window(now, ttl).await {
        Ok(()) => tracing::info!("blackout: window opened at {now} (ttl {ttl}s)"),
        Err(err) => tracing::error!("blackout: failed to open window: {err}"),
    }

    // System 3 — cancel resting entry orders on elevated-spread instruments
    // and store their signed intent for the recovery watcher to re-drive.
    // Shares the `SpreadBlackoutRecord` per trade with System 2's widen
    // (widened stops in `original_stops`, cancelled orders in
    // `cancelled_orders`) and the watcher restores both.
    crate::cancel_resting_orders(store, cron, now).await;
}

/// **System 2** — pre-emptively widen every open position's stop away from
/// price during that instrument's learned **spread hour(s)**, so the spread
/// spike can't clip the stop, remembering each original on a per-trade
/// `SpreadBlackoutRecord` for the recovery watcher to restore.
///
/// # Why this is separate from [`apply_if_ny_close_edge`]
///
/// The widen used to fire only at the single global NY-close edge (21:00/
/// 22:00 UTC). The 2026-07-05 sampler analysis (1183 instruments) showed
/// spread hours are **per-instrument** session boundaries — Gold spreads
/// overnight (18:00–06:00 UTC), EUR/USD spikes at 21:00, indices at their
/// own hours — and the NY-close hour is nearly the emptiest peak. So the
/// widen now gates per-instrument on
/// [`spread_hour_widen_pips`](trade_control_core::spread_blackout::spread_hour_widen_pips):
/// the baked mask says *when* (this instrument's elevated hours, with a
/// ~30-min lead so the stop is out of the way before the top-of-hour spike)
/// and the baked p90 says *how much*. Un-sampled instruments fall back to
/// the legacy `is_ny_close_edge` gate via
/// [`is_spread_hour`](trade_control_core::spread_blackout::is_spread_hour).
///
/// Fires on **every** tick (the caller does not gate it) because the lead
/// window straddles the top of the hour — the per-instrument mask check is
/// cheap and no-ops when no open position is in a spread hour. Applies to
/// **all** open positions including breakeven-locked ones (a breakeven stop
/// sits exactly at entry and is the *most* spread-vulnerable — see the
/// widen decision in `widen_one`).
pub async fn widen_open_stops_for_spread_hours<S, C>(store: &S, cron: &C, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    widen_open_stops(store, cron, now).await;
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
async fn widen_open_stops<S, C>(store: &S, cron: &C, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    let attempts = match store.list_all_entry_attempts().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("blackout widen: list_all_entry_attempts: {err}");
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
    tracing::info!(
        "blackout widen: {} attempt(s), {} affected account(s)",
        attempts.len(),
        accounts.len(),
    );
    for account in accounts {
        widen_account(store, cron, &attempts, account.as_deref(), now).await;
    }
}

/// Widen every open position on one account. Logs + skips per position.
async fn widen_account<S, C>(
    store: &S,
    cron: &C,
    attempts: &[EntryAttempt],
    account: Option<&str>,
    now: DateTime<Utc>,
) where
    S: StateStore,
    C: CronEnv,
{
    let Some(broker) = cron.acquire_broker(account).await else {
        tracing::error!(
            "blackout widen[{}]: broker acquisition failed; skipping account",
            account.unwrap_or("<global>"),
        );
        return;
    };
    let account_id = account.unwrap_or("");
    let positions = match list_positions(&broker, account_id).await {
        Ok(p) => p,
        Err(err) => {
            tracing::error!(
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
async fn widen_one<S: StateStore>(
    store: &S,
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

    // Per-instrument spread-hour gate (cheap; do it before the join/quote).
    // `Some(p90)` ⇒ this instrument IS in (or leading into) a learned spread
    // hour now, and `p90` is the baked widen size. `None` ⇒ either this
    // instrument has baked spread hours but now isn't one of them, OR it's
    // un-sampled — disambiguate with the legacy NY-close-edge fallback so
    // un-sampled assets keep their prior behaviour.
    let baked_p90 = spread_hour_widen_pips(&position.instrument, now);
    if baked_p90.is_none() && !is_ny_close_edge(now) {
        // Not a spread hour for this instrument, and not the legacy fallback
        // edge either → nothing to widen this tick. (Silent: the common case
        // on most ticks for most instruments.)
        return;
    }

    // Join → originating attempt (for trade_id + baked pip_size).
    let Some(attempt) = join_position_to_attempt(position, account, attempts) else {
        tracing::info!(
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
            tracing::info!(
                "blackout widen[{}]: trade {trade_id} already applied; skip",
                account.unwrap_or("<global>"),
            );
            return;
        }
        Ok(_) => {}
        Err(err) => {
            tracing::error!(
                "blackout widen[{}]: get_record({trade_id}): {err}",
                account.unwrap_or("<global>")
            );
            return;
        }
    }

    // Pip from the joined attempt — never widen with a wrong/absent pip.
    let Some(pip_size) = attempt.pip_size.filter(|p| *p > 0.0 && p.is_finite()) else {
        tracing::info!(
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
        tracing::error!(
            "blackout widen[{}]: get_quote({}) failed; skip trade {trade_id}",
            account.unwrap_or("<global>"),
            position.instrument,
        );
        return;
    };
    let spread_pips = spread_abs / pip_size;
    // Widen size: with a baked per-instrument p90 for this spread hour, blend
    // it with the live spread (baked primary, live as a floor, per-instrument
    // ceiling) via `spread_hour_widen_size`. Without one (an un-sampled
    // instrument on the legacy NY-close-edge fallback), keep the flat 22–40
    // `clamp_widen`. See `blackout_widen` for the documented blend rationale.
    let widen_pips = match baked_p90 {
        Some(p90) => spread_hour_widen_size(p90, spread_pips),
        None => clamp_widen(spread_pips),
    };
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
        tracing::error!(
            "blackout widen[{}]: upsert_record({trade_id}) FAILED ({err}); NOT amending (no \
             remembered original ⇒ no widen)",
            account.unwrap_or("<global>"),
        );
        return;
    }

    // PRECONDITION-guarded amend: log the intended amend prominently so a
    // demo run can read it back and confirm `AmendCloseOrder`-on-open-position
    // actually moved the SL (and left TP) before this is trusted live.
    let widen_source = match baked_p90 {
        Some(p90) => format!("baked-p90 {p90:.1}p (spread-hour)"),
        None => "legacy ny-close clamp".to_string(),
    };
    tracing::info!(
        "blackout widen[{}]: INTENT amend_stop trade={trade_id} id={} instrument={} dir={:?} \
         original_sl={original_sl} spread_pips={spread_pips:.1} widen_pips={widen_pips} \
         new_sl={new_sl} via {widen_source} (DEMO-CONFIRM AmendCloseOrder-on-open-position before \
         trusting live)",
        account.unwrap_or("<global>"),
        position.order_id,
        position.instrument,
        position.direction,
    );
    match amend(broker, account.unwrap_or(""), &position.order_id, new_sl).await {
        Ok(()) => tracing::info!(
            "blackout widen[{}]: amend_stop ok trade={trade_id} id={} -> {new_sl}",
            account.unwrap_or("<global>"),
            position.order_id,
        ),
        Err(AmendError::NotFound) => tracing::info!(
            "blackout widen[{}]: amend_stop id={} not found (position closed?) trade={trade_id} — \
             benign; record stays for restore-as-noop / backstop clear",
            account.unwrap_or("<global>"),
            position.order_id,
        ),
        Err(err) => tracing::error!(
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
            blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
            breakeven: None,
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
