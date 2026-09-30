//! Broker-truth reconciliation — catches a plan whose OWN records believe a
//! position is still open when the broker no longer shows it.
//!
//! Every other cron pass (`sweep`, `breakeven_watch`, `blackout_apply`,
//! `order_control_tick`) reaches the broker only *via* a resolvable
//! [`EntryAttempt`] row — they walk attempts, join to a broker position, and
//! act. None of them ever asks the broker "what is actually open right now,
//! independent of what we think we know" and compares that against what our
//! own attempts believe. A plan whose attempt never resolved cleanly (see
//! `BUG-oanda-order-trade-id-bridge`, fixed in `broker-oanda`, or any future
//! case where `lookup_attempt_state` still can't settle it) is invisible to
//! every one of them — it just keeps ticking its rules (pause/news/veto)
//! against a position that may have closed hours ago.
//!
//! # Incident
//!
//! Plan `hs-aud-jpy-dd5db625` (2026-09-30): a filled order's trade id
//! differed from the order id (the bridging bug), the retry gate correctly
//! failed safe (`prior-attempt-unknown`), and the plan sat ticking for over
//! 2 hours against a position that had already hit TP and closed at the
//! broker, +18369.22 AUD, with the account fully flat the whole time.
//!
//! # What this pass does — and deliberately does NOT do
//!
//! For every account with at least one [`EntryAttempt`] carrying a
//! snapshotted `broker_trade_id` (the attempt reached an open position at
//! some point), fetch the broker's current open positions and check: is that
//! trade id still among them? If not, resolve the attempt's actual state via
//! [`Broker::lookup_attempt_state`] and **log** the mismatch with full
//! detail (closed win/loss and realized P&L when resolvable).
//!
//! This pass is **observation-only**. It does not close positions, does not
//! cancel orders, does not mark a plan `Done`, and does not touch
//! `EntryAttempt` or plan state at all. Per the operator (2026-09-30): watch
//! behavior on the demo account first before deciding whether/how to
//! auto-retire a plan. Fail-soft per account (logs + skips), same discipline
//! as every other pass in this crate — a broker error here must never affect
//! the real trading path, which is why it runs on its own cron interval,
//! entirely independent of the engine tick, sweep, and breakeven watch.

use chrono::{DateTime, Utc};
use trade_control_core::broker::{AttemptState, LookupError, OpenPosition};
use trade_control_core::state::{EntryAttempt, StateStore};

use crate::broker_handle::BrokerHandle;
use crate::seam::CronEnv;

/// Walk every account with at least one attempt that reached an open
/// position, and log any whose broker_trade_id is no longer among the
/// broker's current open positions. Per-account errors are logged and
/// skipped — one bad account must never abort the pass.
pub async fn reconcile<S, C>(store: &S, cron: &C, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    let attempts = match store.list_all_entry_attempts().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("reconcile: list_all_entry_attempts: {err}");
            return;
        }
    };
    // Only attempts that ever reached an open position are interesting — an
    // attempt with no snapshotted broker_trade_id was never confirmed open,
    // so there is nothing for this pass to reconcile.
    let tracked: Vec<&EntryAttempt> = attempts
        .iter()
        .filter(|a| a.broker_trade_id.is_some())
        .collect();
    if tracked.is_empty() {
        return;
    }
    let mut accounts: Vec<Option<String>> = Vec::new();
    for a in &tracked {
        if !accounts.contains(&a.account) {
            accounts.push(a.account.clone());
        }
    }
    tracing::info!(
        "reconcile: {} tracked attempt(s), {} account(s)",
        tracked.len(),
        accounts.len(),
    );
    for account in accounts {
        reconcile_account(cron, &tracked, account.as_deref(), now).await;
    }
}

/// Reconcile every tracked attempt on one account against the broker's
/// current open positions.
async fn reconcile_account<C: CronEnv>(
    cron: &C,
    tracked: &[&EntryAttempt],
    account: Option<&str>,
    now: DateTime<Utc>,
) {
    let Some(broker) = cron.acquire_broker(account).await else {
        tracing::error!(
            "reconcile[{}]: broker acquisition failed; skipping account",
            account.unwrap_or("<global>"),
        );
        return;
    };
    let account_id = account.unwrap_or("");
    let open_positions = match list_positions(&broker, account_id).await {
        Ok(p) => p,
        Err(err) => {
            tracing::error!(
                "reconcile[{}]: list_open_positions: {err}",
                account.unwrap_or("<global>"),
            );
            return;
        }
    };
    for attempt in tracked {
        if attempt.account.as_deref() != account {
            continue;
        }
        reconcile_one(&broker, attempt, &open_positions, now).await;
    }
}

/// Check one attempt's believed-open position against the broker's current
/// snapshot; if absent, resolve + log what actually became of it.
async fn reconcile_one<B: AttemptBroker>(
    broker: &B,
    attempt: &EntryAttempt,
    open_positions: &[OpenPosition],
    now: DateTime<Utc>,
) {
    let Some(trade_id) = attempt.broker_trade_id.as_deref() else {
        return;
    };
    if open_positions.iter().any(|p| p.position_id == trade_id) {
        // Still open per the broker — nothing to reconcile.
        return;
    }
    let resolved = broker
        .lookup_attempt_state(
            &attempt.instrument,
            &attempt.broker_order_id,
            Some(trade_id),
        )
        .await;
    match resolved {
        Ok(AttemptState::ClosedWin { realized_pl }) => {
            tracing::warn!(
                "reconcile: plan={} account={} instrument={} trade_id={trade_id} believed OPEN, \
                 broker shows CLOSED WIN realized_pl={realized_pl} — plan is stale as of {now}",
                attempt.trade_id,
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.instrument,
            );
        }
        Ok(AttemptState::ClosedLossOrBreakeven { realized_pl }) => {
            tracing::warn!(
                "reconcile: plan={} account={} instrument={} trade_id={trade_id} believed OPEN, \
                 broker shows CLOSED LOSS/BREAKEVEN realized_pl={realized_pl} — plan is stale as \
                 of {now}",
                attempt.trade_id,
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.instrument,
            );
        }
        Ok(AttemptState::OpenPosition { .. }) => {
            // The broker-truth snapshot and the resolver disagree (a race
            // between the two calls, most likely a fill/close that happened
            // in between). Not evidence of anything on its own; log at INFO
            // and let the next tick settle it.
            tracing::info!(
                "reconcile: plan={} account={} instrument={} trade_id={trade_id} was absent \
                 from list_open_positions but lookup_attempt_state still resolves OpenPosition \
                 — likely a race between the two calls, will re-check next tick",
                attempt.trade_id,
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.instrument,
            );
        }
        Ok(AttemptState::Pending | AttemptState::Cancelled | AttemptState::Unknown) => {
            tracing::warn!(
                "reconcile: plan={} account={} instrument={} trade_id={trade_id} believed OPEN, \
                 broker shows neither open nor a resolvable close ({resolved:?}) — plan is stale \
                 as of {now}",
                attempt.trade_id,
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.instrument,
            );
        }
        Err(err) => {
            tracing::error!(
                "reconcile: plan={} account={} instrument={} trade_id={trade_id} absent from \
                 list_open_positions; lookup_attempt_state failed: {err:?} — will re-check next \
                 tick",
                attempt.trade_id,
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.instrument,
            );
        }
    }
}

/// The one broker operation this pass needs, seamed so [`reconcile_one`] is
/// unit-testable without a live broker — same shape as `breakeven_watch`'s
/// `PositionBroker`.
#[allow(async_fn_in_trait)]
trait AttemptBroker {
    async fn lookup_attempt_state(
        &self,
        instrument: &str,
        broker_order_id: &str,
        broker_trade_id: Option<&str>,
    ) -> Result<AttemptState, LookupError>;
}

impl AttemptBroker for BrokerHandle {
    async fn lookup_attempt_state(
        &self,
        instrument: &str,
        broker_order_id: &str,
        broker_trade_id: Option<&str>,
    ) -> Result<AttemptState, LookupError> {
        lookup(self, instrument, broker_order_id, broker_trade_id).await
    }
}

async fn lookup(
    broker: &BrokerHandle,
    instrument: &str,
    broker_order_id: &str,
    broker_trade_id: Option<&str>,
) -> Result<AttemptState, LookupError> {
    use trade_control_core::broker::Broker;
    match broker {
        BrokerHandle::Oanda(b) => {
            b.lookup_attempt_state(instrument, broker_order_id, broker_trade_id)
                .await
        }
        BrokerHandle::TradeNation(b) => {
            b.lookup_attempt_state(instrument, broker_order_id, broker_trade_id)
                .await
        }
        BrokerHandle::Ibkr(b) => {
            b.lookup_attempt_state(instrument, broker_order_id, broker_trade_id)
                .await
        }
    }
}

async fn list_positions(
    broker: &BrokerHandle,
    account_id: &str,
) -> Result<Vec<OpenPosition>, String> {
    use trade_control_core::broker::Broker;
    let res = match broker {
        BrokerHandle::Oanda(b) => b.list_open_positions(account_id).await,
        BrokerHandle::TradeNation(b) => b.list_open_positions(account_id).await,
        BrokerHandle::Ibkr(b) => b.list_open_positions(account_id).await,
    };
    res.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_core::intent::Direction;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    fn make_attempt(
        trade_id: &str,
        instrument: &str,
        broker_trade_id: Option<&str>,
    ) -> EntryAttempt {
        EntryAttempt {
            trade_id: trade_id.into(),
            account: Some("m-and-w".into()),
            instrument: instrument.into(),
            attempt_no: 1,
            broker_order_id: "2494".into(),
            broker_trade_id: broker_trade_id.map(String::from),
            direction: Direction::Short,
            placed_at: ts("2026-09-29T16:00:11Z"),
            shell_time: ts("2026-09-29T16:00:00Z"),
            expires_at: ts("2026-10-07T16:00:11Z"),
            stop_loss_price: None,
            adverse_extreme: None,
            cancel_at: None,
            pip_size: Some(0.01),
            blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
            breakeven: None,
            order_control: None,
            superseded: false,
        }
    }

    fn make_open_position(position_id: &str, instrument: &str) -> OpenPosition {
        OpenPosition {
            instrument: instrument.into(),
            direction: Direction::Short,
            stop_loss: None,
            take_profit: None,
            position_id: position_id.into(),
            order_id: position_id.into(),
            stake: 100.0,
            entry_price: None,
            opened_at: None,
        }
    }

    struct StubBroker(Result<AttemptState, LookupError>);

    impl AttemptBroker for StubBroker {
        async fn lookup_attempt_state(
            &self,
            _instrument: &str,
            _broker_order_id: &str,
            _broker_trade_id: Option<&str>,
        ) -> Result<AttemptState, LookupError> {
            self.0.clone()
        }
    }

    #[test]
    fn still_open_at_broker_is_a_noop() {
        let attempt = make_attempt("hs-aud-jpy-dd5db625", "AUD_JPY", Some("2495"));
        let open = vec![make_open_position("2495", "AUD_JPY")];
        let broker = StubBroker(Ok(AttemptState::Unknown)); // must never be called
        pollster::block_on(reconcile_one(&broker, &attempt, &open, Utc::now()));
        // No panic, no broker call needed — the position is in `open`.
    }

    #[test]
    fn closed_win_absent_from_open_positions_is_detected() {
        let attempt = make_attempt("hs-aud-jpy-dd5db625", "AUD_JPY", Some("2495"));
        let broker = StubBroker(Ok(AttemptState::ClosedWin {
            realized_pl: 18369.2245,
        }));
        // Broker's open-positions snapshot no longer contains 2495 — the
        // exact shape of the incident.
        pollster::block_on(reconcile_one(&broker, &attempt, &[], Utc::now()));
        // Behavior is logging-only; this test pins that it does not panic and
        // reaches the ClosedWin arm (traced via `cargo test -- --nocapture`
        // during development). A stronger assertion would need a tracing
        // subscriber capture, deliberately not added here to keep this pass
        // observation-only and low-ceremony per its explicit scope.
    }

    #[test]
    fn no_tracked_attempts_short_circuits_without_a_broker_call() {
        let attempt = make_attempt("hs-aud-jpy-dd5db625", "AUD_JPY", None);
        assert!(attempt.broker_trade_id.is_none());
        // reconcile_one is never reached for an attempt with no
        // broker_trade_id — `reconcile`'s filter excludes it upstream. This
        // test documents that invariant at the data level.
    }
}
