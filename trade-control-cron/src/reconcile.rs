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
//! Two groups of attempts, two checks:
//!
//! 1. **Tracked** — [`EntryAttempt`]s carrying a snapshotted
//!    `broker_trade_id` (the attempt reached an open position at some
//!    point). Fetch the broker's current open positions and check: is that
//!    trade id still among them? If not, resolve via
//!    [`Broker::lookup_attempt_state`] and log the mismatch with full detail
//!    (closed win/loss and realized P&L when resolvable).
//! 2. **Unresolved** — attempts with **no** `broker_trade_id` at all. This is
//!    the normal shape of a still-resting order, but it is *also* the exact
//!    shape the incident's own row was left in: the bridging bug meant
//!    `broker_trade_id` was never captured even though the order filled and
//!    later closed, and once its `05-enter` window expires nothing ever
//!    re-probes it again — a plan can sit stuck in `await_entry` for the
//!    rest of its `expires_at` life with no trigger left that would notice
//!    (`trade-expiry` is the only remaining exit, and that can be a day or
//!    more away). For these, call `lookup_attempt_state(order_id, None)`
//!    directly and log **only** the surprising outcomes —
//!    `ClosedWin`/`ClosedLossOrBreakeven` (it secretly filled and closed) —
//!    since `Pending`/`Cancelled`/`Unknown` are the ordinary, already-visible
//!    states for an attempt that was never confirmed open and logging those
//!    every tick would just be noise on top of what the retry gate already
//!    reports.
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
use trade_control_core::recording::{CronNote, CronNoteSeverity};
use trade_control_core::state::{EntryAttempt, StateStore};

use crate::broker_handle::BrokerHandle;
use crate::seam::CronEnv;

/// Who wrote this note, for `CronNote::source` — matched 1:1 against calls to
/// `cron.record_cron_note`, so a reader grepping `source=reconcile` finds
/// every note this pass ever wrote.
const SOURCE: &str = "reconcile";

/// Build + record a [`CronNote`] alongside the `tracing` call every finding
/// already makes. `journal`'s secondary visibility, never a replacement for
/// `journalctl` — see the module docs.
fn note<C: CronEnv>(
    cron: &C,
    attempt: &EntryAttempt,
    severity: CronNoteSeverity,
    now: DateTime<Utc>,
    message: String,
) {
    cron.record_cron_note(CronNote {
        ts: now.to_rfc3339(),
        trade_id: attempt.trade_id.clone(),
        account: attempt.account.clone(),
        source: SOURCE.to_string(),
        severity,
        message,
    });
}

/// Walk every account with at least one interesting attempt — tracked
/// (reached an open position at some point) or unresolved (no
/// `broker_trade_id` ever snapshotted) — and reconcile each against the
/// broker. Per-account errors are logged and skipped — one bad account must
/// never abort the pass.
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
    let tracked: Vec<&EntryAttempt> = attempts
        .iter()
        .filter(|a| a.broker_trade_id.is_some())
        .collect();
    let unresolved: Vec<&EntryAttempt> = attempts
        .iter()
        .filter(|a| a.broker_trade_id.is_none())
        .collect();
    if tracked.is_empty() && unresolved.is_empty() {
        return;
    }
    let mut accounts: Vec<Option<String>> = Vec::new();
    for a in tracked.iter().chain(unresolved.iter()) {
        if !accounts.contains(&a.account) {
            accounts.push(a.account.clone());
        }
    }
    tracing::info!(
        "reconcile: {} tracked, {} unresolved attempt(s), {} account(s)",
        tracked.len(),
        unresolved.len(),
        accounts.len(),
    );
    for account in accounts {
        reconcile_account(cron, &tracked, &unresolved, account.as_deref(), now).await;
    }
}

/// Reconcile every tracked + unresolved attempt on one account against the
/// broker.
async fn reconcile_account<C: CronEnv>(
    cron: &C,
    tracked: &[&EntryAttempt],
    unresolved: &[&EntryAttempt],
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
        reconcile_one(cron, &broker, attempt, &open_positions, now).await;
    }
    for attempt in unresolved {
        if attempt.account.as_deref() != account {
            continue;
        }
        reconcile_unresolved(cron, &broker, attempt, now).await;
    }
}

/// Check one attempt's believed-open position against the broker's current
/// snapshot; if absent, resolve + log what actually became of it.
async fn reconcile_one<C: CronEnv, B: AttemptBroker>(
    cron: &C,
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
            let message = format!(
                "trade_id={trade_id} believed OPEN, broker shows CLOSED WIN \
                 realized_pl={realized_pl} — plan is stale as of {now}"
            );
            tracing::warn!(
                "reconcile: plan={} account={} instrument={} {message}",
                attempt.trade_id,
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.instrument,
            );
            note(cron, attempt, CronNoteSeverity::Warn, now, message);
        }
        Ok(AttemptState::ClosedLossOrBreakeven { realized_pl }) => {
            let message = format!(
                "trade_id={trade_id} believed OPEN, broker shows CLOSED LOSS/BREAKEVEN \
                 realized_pl={realized_pl} — plan is stale as of {now}"
            );
            tracing::warn!(
                "reconcile: plan={} account={} instrument={} {message}",
                attempt.trade_id,
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.instrument,
            );
            note(cron, attempt, CronNoteSeverity::Warn, now, message);
        }
        Ok(AttemptState::OpenPosition { .. }) => {
            // The broker-truth snapshot and the resolver disagree (a race
            // between the two calls, most likely a fill/close that happened
            // in between). Not evidence of anything on its own; log at INFO
            // and let the next tick settle it. No CronNote — not surprising
            // enough to put in front of the operator, same reasoning as
            // `reconcile_unresolved`'s silent arms.
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
            let message = format!(
                "trade_id={trade_id} believed OPEN, broker shows neither open nor a resolvable \
                 close ({resolved:?}) — plan is stale as of {now}"
            );
            tracing::warn!(
                "reconcile: plan={} account={} instrument={} {message}",
                attempt.trade_id,
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.instrument,
            );
            note(cron, attempt, CronNoteSeverity::Warn, now, message);
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

/// Check one attempt that never had a `broker_trade_id` snapshotted — the
/// normal shape of a still-resting order, but also the shape left behind by
/// the order/trade id bridging bug (`fix/oanda-order-trade-id-bridge`): the
/// order filled and even closed, and nothing ever captured that. Probe the
/// broker directly and log only the surprising outcomes; `Pending` /
/// `Cancelled` / `Unknown` are the ordinary states for an attempt that was
/// never confirmed open and are already visible via the retry gate's own
/// `prior-attempt-unknown` rejections, so logging them here on every tick
/// would just be noise.
async fn reconcile_unresolved<C: CronEnv, B: AttemptBroker>(
    cron: &C,
    broker: &B,
    attempt: &EntryAttempt,
    now: DateTime<Utc>,
) {
    let resolved = broker
        .lookup_attempt_state(&attempt.instrument, &attempt.broker_order_id, None)
        .await;
    match resolved {
        Ok(AttemptState::ClosedWin { realized_pl }) => {
            let message = format!(
                "order_id={} never had a broker_trade_id snapshotted, broker shows it filled \
                 and CLOSED WIN realized_pl={realized_pl} — plan is stale as of {now}",
                attempt.broker_order_id,
            );
            tracing::warn!(
                "reconcile: plan={} account={} instrument={} {message}",
                attempt.trade_id,
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.instrument,
            );
            note(cron, attempt, CronNoteSeverity::Warn, now, message);
        }
        Ok(AttemptState::ClosedLossOrBreakeven { realized_pl }) => {
            let message = format!(
                "order_id={} never had a broker_trade_id snapshotted, broker shows it filled \
                 and CLOSED LOSS/BREAKEVEN realized_pl={realized_pl} — plan is stale as of {now}",
                attempt.broker_order_id,
            );
            tracing::warn!(
                "reconcile: plan={} account={} instrument={} {message}",
                attempt.trade_id,
                attempt.account.as_deref().unwrap_or("<global>"),
                attempt.instrument,
            );
            note(cron, attempt, CronNoteSeverity::Warn, now, message);
        }
        // Pending / Cancelled / Unknown / OpenPosition / Err: the ordinary
        // or already-visible cases — see the function doc for why these
        // deliberately don't log, and therefore don't note either.
        _ => {}
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

    /// A `CronEnv` that records every `CronNote` it's given, so a test can
    /// assert whether `reconcile_one`/`reconcile_unresolved` actually wrote
    /// one (and what it said) — not just that the function ran without
    /// panicking. `acquire_broker`/`dispatch_config` panic: `reconcile_one`
    /// and `reconcile_unresolved` never call either, so a reachable panic
    /// there is itself a test failure.
    #[derive(Default)]
    struct RecordingCronEnv {
        notes: std::cell::RefCell<Vec<CronNote>>,
    }

    impl CronEnv for RecordingCronEnv {
        async fn acquire_broker(&self, _account: Option<&str>) -> Option<BrokerHandle> {
            panic!("reconcile_one/reconcile_unresolved must never acquire a broker directly")
        }
        async fn dispatch_config(
            &self,
            _verified: &trade_control_core::incoming::Verified,
        ) -> trade_control_core::dispatch_config::DispatchConfig {
            unreachable!("not used by reconcile")
        }
        fn record_tick(&self, _bundle: trade_control_core::tick_bundle::TickBundle) {
            unreachable!("reconcile never evaluates a plan tick")
        }
        fn record_cron_note(&self, note: CronNote) {
            self.notes.borrow_mut().push(note);
        }
        fn signing_key(&self) -> Option<Vec<u8>> {
            None
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
        let cron = RecordingCronEnv::default();
        pollster::block_on(reconcile_one(&cron, &broker, &attempt, &open, Utc::now()));
        // No panic, no broker call needed — the position is in `open`.
        assert!(cron.notes.borrow().is_empty());
    }

    #[test]
    fn closed_win_absent_from_open_positions_is_detected() {
        let attempt = make_attempt("hs-aud-jpy-dd5db625", "AUD_JPY", Some("2495"));
        let broker = StubBroker(Ok(AttemptState::ClosedWin {
            realized_pl: 18369.2245,
        }));
        let cron = RecordingCronEnv::default();
        // Broker's open-positions snapshot no longer contains 2495 — the
        // exact shape of the incident.
        pollster::block_on(reconcile_one(&cron, &broker, &attempt, &[], Utc::now()));
        let notes = cron.notes.borrow();
        assert_eq!(
            notes.len(),
            1,
            "a closed-win mismatch must write exactly one note"
        );
        assert_eq!(notes[0].trade_id, "hs-aud-jpy-dd5db625");
        assert_eq!(notes[0].source, "reconcile");
        assert_eq!(notes[0].severity, CronNoteSeverity::Warn);
        assert!(
            notes[0].message.contains("18369.2245"),
            "message must carry the realized P&L: {:?}",
            notes[0].message
        );
    }

    /// Records whether `lookup_attempt_state` was called, so a test can
    /// assert the broker WAS or WASN'T reached — `reconcile_one`'s
    /// still-open short circuit must never make the call at all.
    struct CountingBroker {
        result: Result<AttemptState, LookupError>,
        calls: std::cell::Cell<u32>,
    }

    impl CountingBroker {
        fn new(result: Result<AttemptState, LookupError>) -> Self {
            Self {
                result,
                calls: std::cell::Cell::new(0),
            }
        }
    }

    impl AttemptBroker for CountingBroker {
        async fn lookup_attempt_state(
            &self,
            _instrument: &str,
            _broker_order_id: &str,
            _broker_trade_id: Option<&str>,
        ) -> Result<AttemptState, LookupError> {
            self.calls.set(self.calls.get() + 1);
            self.result.clone()
        }
    }

    #[test]
    fn no_tracked_attempts_short_circuits_without_a_broker_call() {
        let attempt = make_attempt("hs-aud-jpy-dd5db625", "AUD_JPY", Some("2495"));
        let open = vec![make_open_position("2495", "AUD_JPY")];
        let broker = CountingBroker::new(Ok(AttemptState::Unknown));
        let cron = RecordingCronEnv::default();
        pollster::block_on(reconcile_one(&cron, &broker, &attempt, &open, Utc::now()));
        assert_eq!(
            broker.calls.get(),
            0,
            "a position still present in list_open_positions must never trigger a lookup"
        );
        assert!(cron.notes.borrow().is_empty());
    }

    /// THE INCIDENT, exactly: an attempt with no `broker_trade_id` ever
    /// snapshotted (the bridging bug's aftermath — `sweep`'s own
    /// `snapshot_broker_trade_id` only writes it on `OpenPosition`, and this
    /// attempt's trade closed before that fix landed, so it never will) must
    /// still be probed and its closed state logged.
    #[test]
    fn unresolved_attempt_that_secretly_closed_is_probed_and_detected() {
        let attempt = make_attempt("hs-aud-jpy-dd5db625", "AUD_JPY", None);
        let broker = CountingBroker::new(Ok(AttemptState::ClosedWin {
            realized_pl: 18369.2245,
        }));
        let cron = RecordingCronEnv::default();
        pollster::block_on(reconcile_unresolved(&cron, &broker, &attempt, Utc::now()));
        assert_eq!(
            broker.calls.get(),
            1,
            "an unresolved attempt must be probed via lookup_attempt_state"
        );
        let notes = cron.notes.borrow();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].severity, CronNoteSeverity::Warn);
        assert!(notes[0].message.contains("18369.2245"));
    }

    /// The ordinary case — a still-resting order — must be probed (there is
    /// no cheaper way to tell it apart from the incident's shape) but must
    /// NOT be treated as anything worth flagging. This test only pins that
    /// the call happens; the absence of a WARN log for `Pending` is asserted
    /// by inspection of `reconcile_unresolved`'s match arms (the `_ => {}`
    /// catch-all), not re-derived here via a tracing capture — see the
    /// `closed_win_absent_from_open_positions_is_detected` note above on
    /// keeping this pass's tests low-ceremony.
    #[test]
    fn unresolved_attempt_still_pending_is_probed_but_not_flagged() {
        let attempt = make_attempt("hs-aud-jpy-dd5db625", "AUD_JPY", None);
        let broker = CountingBroker::new(Ok(AttemptState::Pending));
        let cron = RecordingCronEnv::default();
        pollster::block_on(reconcile_unresolved(&cron, &broker, &attempt, Utc::now()));
        assert_eq!(broker.calls.get(), 1);
        assert!(
            cron.notes.borrow().is_empty(),
            "Pending must not write a note — see the function doc for why"
        );
    }
}
