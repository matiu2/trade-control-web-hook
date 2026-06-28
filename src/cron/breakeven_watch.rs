//! Break-even stop watcher (BUG-replay-no-breakeven-stop-at-50pct).
//!
//! Runs every 15-min cron tick alongside the order sweep and the spread
//! watcher. For each open position whose originating enter carried a
//! `breakeven` rule (snapshotted onto its [`EntryAttempt`] at placement), it
//! moves the broker-native stop-loss to **break-even** (the entry price) once a
//! candle has **closed** past 50% of the way from entry to take-profit.
//!
//! The worker has no per-position event loop, so this cron *is* the live
//! consumer of the break-even rule — the replay's counterpart is
//! `engine::simulate_fill`, which walks the candle path directly. Both resolve
//! the decision through the same pure core helper
//! ([`Breakeven::decide_move`](trade_control_core::intent::Breakeven::decide_move))
//! so the two can't drift (the standing "strategy changes go in BOTH replayer +
//! worker" rule).
//!
//! Mechanism (mirrors `blackout_apply`'s widen, but the *other direction* —
//! tightening to entry, not widening away):
//!
//! 1. List all [`EntryAttempt`] rows; the accounts with a tracked entry *are*
//!    the accounts with joinable open positions (self-scoping).
//! 2. For each open position, join back to its attempt and read the baked
//!    [`BreakevenSnapshot`] (entry / TP / threshold / granularity).
//! 3. Fetch the closed candles since the fill, find the one that ran furthest
//!    toward TP, and ask the pure helper for the new stop.
//! 4. If armed and not already at break-even, `amend_stop(entry)`.
//!
//! Idempotency / one-way: the decision returns `None` when the stop is already
//! at (or past, in the trade's favour) break-even, so re-running every tick is
//! a no-op once armed — no need to persist an "armed" flag. The move never
//! widens a stop (the helper only ever returns the entry price, and suppresses
//! the move when `current_stop` is already there).
//!
//! **PRECONDITION (shared with `blackout_apply`):** `amend_stop` on an OPEN
//! position via TradeNation's `AmendCloseOrder` is demo-unverified. Every
//! intended amend is logged prominently first so a demo run can read it back
//! (SL moved to entry, TP unchanged) before this is trusted live.

use chrono::{DateTime, Duration, Utc};
use trade_control_core::broker::{AmendError, Broker, Candle, Granularity, OpenPosition};
use trade_control_core::state::{BreakevenSnapshot, EntryAttempt, StateStore};
use worker::Env;

use super::sweep::{BrokerHandle, acquire_broker_for_account, open_store};

/// How far back to look for closed candles when deciding a break-even arm. The
/// arm is latched and idempotent, so we only need to catch a candle that closed
/// past 50% at any point since the fill — but the broker candle pull is bounded,
/// so we look back a generous window of the trade's own bars.
const BREAKEVEN_LOOKBACK_BARS: i64 = 500;

/// Walk every open position and move its stop to break-even when a candle has
/// closed past 50%-to-TP. Per-row errors are logged and skipped — one bad row
/// must never abort the loop (same discipline as the order sweep / blackout).
pub async fn watch(env: &Env, now: DateTime<Utc>) {
    let Some(store) = open_store(env) else {
        return;
    };
    let attempts = match store.list_all_entry_attempts().await {
        Ok(v) => v,
        Err(err) => {
            rlog_err!("breakeven watch: list_all_entry_attempts: {err}");
            return;
        }
    };
    // Only trades that opted into break-even are interesting; if none did, skip
    // the broker round-trips entirely.
    if !attempts.iter().any(|a| a.breakeven.is_some()) {
        return;
    }
    // Distinct affected accounts (preserves insertion order, dedups).
    let mut accounts: Vec<Option<String>> = Vec::new();
    for a in &attempts {
        if a.breakeven.is_some() && !accounts.contains(&a.account) {
            accounts.push(a.account.clone());
        }
    }
    rlog!(
        "breakeven watch: {} attempt(s), {} BE account(s)",
        attempts.len(),
        accounts.len(),
    );
    for account in accounts {
        watch_account(env, &attempts, account.as_deref(), now).await;
    }
}

/// Move-to-BE every eligible open position on one account. Logs + skips per
/// position.
async fn watch_account(
    env: &Env,
    attempts: &[EntryAttempt],
    account: Option<&str>,
    now: DateTime<Utc>,
) {
    let Some(broker) = acquire_broker_for_account(env, account).await else {
        rlog_err!(
            "breakeven watch[{}]: broker acquisition failed; skipping account",
            account.unwrap_or("<global>"),
        );
        return;
    };
    let account_id = account.unwrap_or("");
    let positions = match list_positions(&broker, account_id).await {
        Ok(p) => p,
        Err(err) => {
            rlog_err!(
                "breakeven watch[{}]: list_open_positions: {err}",
                account.unwrap_or("<global>"),
            );
            return;
        }
    };
    for position in positions {
        watch_one(&broker, account, attempts, &position, now).await;
    }
}

/// Decide + (maybe) move one open position's stop to break-even.
async fn watch_one(
    broker: &BrokerHandle,
    account: Option<&str>,
    attempts: &[EntryAttempt],
    position: &OpenPosition,
    now: DateTime<Utc>,
) {
    // A position with no attached stop is left alone — break-even moves an
    // existing stop, it doesn't add one (same stance as the blackout widen).
    let Some(current_stop) = position.stop_loss else {
        return;
    };
    // Join → originating attempt → its break-even snapshot. Positions whose
    // attempt opted out (or that don't join) are skipped silently.
    let Some(attempt) = join_position_to_attempt(position, account, attempts) else {
        return;
    };
    let Some(snap) = attempt.breakeven else {
        return;
    };
    let trade_id = attempt.trade_id.as_str();

    // Fetch the closed candles since (around) the fill at the trade's
    // granularity, and find the close that ran furthest toward TP.
    let Some(best_close) =
        best_close_toward_tp(broker, &position.instrument, &snap, position, now).await
    else {
        // No usable closed candle yet (just filled, or the pull failed) — try
        // again next tick. Not an error worth shouting about every position.
        return;
    };

    let Some(new_stop) = snap.rule.decide_move(
        position.direction,
        snap.entry_price,
        snap.take_profit,
        current_stop,
        best_close,
    ) else {
        // Not armed yet, or already at break-even → nothing to do.
        return;
    };

    // PRECONDITION-guarded amend: log the intent prominently so a demo run can
    // confirm `AmendCloseOrder`-on-open-position moved the SL (and left TP)
    // before this is trusted live.
    rlog!(
        "breakeven watch[{}]: INTENT amend_stop trade={trade_id} id={} instrument={} dir={:?} \
         current_sl={current_stop} -> BE={new_stop} (entry={}, tp={}, best_close={best_close}) \
         (DEMO-CONFIRM AmendCloseOrder-on-open-position before trusting live)",
        account.unwrap_or("<global>"),
        position.order_id,
        position.instrument,
        position.direction,
        snap.entry_price,
        snap.take_profit,
    );
    match amend(broker, account.unwrap_or(""), &position.order_id, new_stop).await {
        Ok(()) => rlog!(
            "breakeven watch[{}]: amend_stop ok trade={trade_id} id={} -> {new_stop} (break-even)",
            account.unwrap_or("<global>"),
            position.order_id,
        ),
        Err(AmendError::NotFound) => rlog!(
            "breakeven watch[{}]: amend_stop id={} not found (position closed?) trade={trade_id} — \
             benign",
            account.unwrap_or("<global>"),
            position.order_id,
        ),
        Err(err) => rlog_err!(
            "breakeven watch[{}]: amend_stop trade={trade_id} id={} -> {new_stop} FAILED ({err}); \
             will retry next tick",
            account.unwrap_or("<global>"),
            position.order_id,
        ),
    }
}

/// Fetch the closed candles since the fill at the snapshot's granularity, and
/// fold them into the close that ran *furthest toward TP* — the input the pure
/// helper needs (a BE arm on a bar that has since retraced must not be missed,
/// since BE is latched). Returns `None` when no closed candle is available.
async fn best_close_toward_tp(
    broker: &BrokerHandle,
    instrument: &str,
    snap: &BreakevenSnapshot,
    position: &OpenPosition,
    now: DateTime<Utc>,
) -> Option<f64> {
    let since = now - Duration::seconds(snap.granularity.seconds() * BREAKEVEN_LOOKBACK_BARS);
    let candles = fetch_candles(broker, instrument, snap.granularity, since, now).await?;
    // Only *closed* candles count ("a candle CLOSES past 50%"): a bar whose
    // open time + one granularity is still in the future hasn't closed yet.
    let bar = Duration::seconds(snap.granularity.seconds());
    candles
        .into_iter()
        .filter(|c| c.time + bar <= now)
        .map(|c| c.c)
        .reduce(|a, b| {
            trade_control_core::intent::Breakeven::more_progressed(position.direction, a, b)
        })
}

async fn fetch_candles(
    broker: &BrokerHandle,
    instrument: &str,
    granularity: Granularity,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<Vec<Candle>> {
    let res = match broker {
        BrokerHandle::Oanda(b) => b.get_candles(instrument, granularity, since, now).await,
        BrokerHandle::TradeNation(b) => b.get_candles(instrument, granularity, since, now).await,
    };
    match res {
        Ok(c) => Some(c),
        Err(err) => {
            rlog_err!("breakeven watch: get_candles({instrument}): {err}");
            None
        }
    }
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

/// Join an open position to the [`EntryAttempt`] that placed it — the source of
/// the baked [`BreakevenSnapshot`]. Same principled match + coarse fallback as
/// `blackout_apply::join_position_to_attempt`: exact on the snapshotted
/// `broker_trade_id == position_id`, else `instrument + direction + account`.
/// Pure & unit-testable.
fn join_position_to_attempt<'a>(
    position: &OpenPosition,
    account: Option<&str>,
    attempts: &'a [EntryAttempt],
) -> Option<&'a EntryAttempt> {
    if let Some(hit) = attempts
        .iter()
        .find(|a| a.broker_trade_id.as_deref() == Some(position.position_id.as_str()))
    {
        return Some(hit);
    }
    attempts.iter().find(|a| {
        a.instrument == position.instrument
            && a.direction == position.direction
            && a.account.as_deref() == account
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_core::intent::{Breakeven, Direction};

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    fn attempt_with_be(
        instrument: &str,
        direction: Direction,
        account: Option<&str>,
        broker_trade_id: Option<&str>,
        snap: Option<BreakevenSnapshot>,
    ) -> EntryAttempt {
        EntryAttempt {
            trade_id: "t1".into(),
            account: account.map(|s| s.into()),
            instrument: instrument.into(),
            attempt_no: 1,
            broker_order_id: "ord-1".into(),
            broker_trade_id: broker_trade_id.map(|s| s.into()),
            direction,
            placed_at: ts("2026-06-24T00:00:00Z"),
            shell_time: ts("2026-06-24T00:00:00Z"),
            expires_at: ts("2026-06-30T00:00:00Z"),
            stop_loss_price: Some(1.1040),
            cancel_at: None,
            pip_size: Some(0.0001),
            blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
            breakeven: snap,
        }
    }

    fn position(instrument: &str, direction: Direction, position_id: &str) -> OpenPosition {
        OpenPosition {
            instrument: instrument.into(),
            direction,
            stop_loss: Some(1.1040),
            take_profit: None,
            position_id: position_id.into(),
            order_id: "ord-1".into(),
            stake: 1.0,
        }
    }

    #[test]
    fn join_matches_on_broker_trade_id_first() {
        let snap = BreakevenSnapshot {
            rule: Breakeven::at_half(),
            entry_price: 1.1000,
            take_profit: 1.0900,
            granularity: Granularity::H1,
        };
        let attempts = vec![attempt_with_be(
            "EUR_USD",
            Direction::Short,
            Some("reversals"),
            Some("POS-9"),
            Some(snap),
        )];
        let pos = position("EUR_USD", Direction::Short, "POS-9");
        let hit = join_position_to_attempt(&pos, Some("reversals"), &attempts).unwrap();
        assert!(hit.breakeven.is_some());
    }

    #[test]
    fn join_misses_when_nothing_correlates() {
        let attempts = vec![attempt_with_be(
            "EUR_USD",
            Direction::Long,
            Some("reversals"),
            None,
            None,
        )];
        let pos = position("EUR_NZD", Direction::Short, "POS-X");
        assert!(join_position_to_attempt(&pos, Some("reversals"), &attempts).is_none());
    }
}
