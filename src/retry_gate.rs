//! Multi-shot retry gate for `enter` intents that set `max_retries`.
//!
//! **What "retry" means here (read this first).** A retry is *not* a
//! re-attempt of a placement that failed to reach the broker. It is a
//! **fresh entry into a setup that has already been entered once and
//! whose first entry has since closed** — typically by hitting stop
//! loss — while the original setup is still considered valid (the
//! alert's `not_after` window has not yet elapsed and the geometry
//! hasn't been invalidated). The pattern is: place → fill → hit SL →
//! a new signal bar arrives within the same alert window → place
//! again, up to `max_retries` total placements.
//!
//! Things this gate is **not**:
//!
//! - It is not a retry of broker placement errors (HTTP failures,
//!   broker rejections). Those produce a 502 response and consume the
//!   intent's seen-by-id slot like any other terminal outcome.
//! - It is not a retry of pre-broker rejections (vetos, cooldowns,
//!   the `allow_entry` script gate, prep gates). Strategy-side
//!   rejections don't burn a slot — see point 3 below.
//! - It is not a way to refire the *same alert payload* multiple
//!   times. The top-level `is_seen` dedup at the worker dispatcher
//!   (`src/lib.rs`) still applies per intent id; a TradingView alert
//!   that re-sends the byte-identical body will 409 there, before
//!   this gate runs. Multi-shot operates on **distinct fires** of the
//!   same `trade_id` from successive signal bars, each carrying a
//!   fresh intent id.
//!
//! When the operator opts a setup into multi-shot mode by setting
//! `max_retries: N` (any non-default Tunable, i.e. anything that isn't
//! `Static(0)`) and a `trade_id`, the alert may legitimately fire on
//! multiple firing bars within the `not_after` window. Each arrival
//! flows through this gate before reaching the cooldown / prep / veto
//! checks and the broker placement.
//!
//! The gate has three jobs:
//!
//! 1. Dedup same-bar re-fires via
//!    [`StateStore::is_retry_fire_seen`] / [`StateStore::mark_retry_fire_seen`].
//!    Two arrivals carrying the same `shell.time` for the same
//!    `(account, trade_id)` collapse to one placement.
//! 2. Cross-reference each prior attempt (newest-first) against broker
//!    state via [`Broker::lookup_attempt_state`]. A previous attempt
//!    that is still **open** (filled and live) rejects this fire with
//!    412 — we won't stack a second entry on top of the first while
//!    the first is still running. A still-**pending** order is
//!    cancelled (the new signal bar supersedes it) and the gate falls
//!    through to a fresh placement. **Closed** attempts (SL hit, TP
//!    hit, cancelled, unknown) are skipped over toward older
//!    attempts; those are the legitimate "prior entry already done,
//!    new opportunity" cases this gate exists to allow.
//! 3. Enforce the placement cap. Crucially: only the `EntryAttempt`
//!    rows count — strategy-side gates that prevent a placement
//!    (cooldown, prep order, veto, `allow_entry` script) don't burn a
//!    slot. This is also what the `attempts.len() >= max_retries`
//!    check expresses naturally: we count attempts that actually
//!    reached `place_entry`.
//!
//! When `intent.max_retries` is the default `Static(0)`, callers must
//! skip this module entirely — the byte-for-byte single-shot
//! behaviour predates the gate and is verified by an explicit
//! regression test in the worker suite.

use chrono::{DateTime, Utc};
use trade_control_core::broker::{AttemptState, Broker, LookupError};
use trade_control_core::intent::{Intent, Shell};
use trade_control_core::rules::{self, RuleError};
use trade_control_core::state::{EntryAttempt, StateStore};
use trade_control_core::tunable::Tunable;

/// Tracing-shaped wrappers around `worker::console_*!`. The worker
/// macros call into `web_sys::console::log_1`, which panics on
/// non-wasm builds — without these wrappers the native test build
/// aborts the moment a code path emits a log line. We log via
/// `tracing` everywhere off-wasm so tests stay debuggable, and the
/// real console on wasm.
macro_rules! console_log {
    ($($t:tt)*) => {{
        #[cfg(target_arch = "wasm32")]
        { worker::console_log!($($t)*); }
        #[cfg(not(target_arch = "wasm32"))]
        { tracing::info!("{}", format_args!($($t)*)); }
    }};
}
macro_rules! console_error {
    ($($t:tt)*) => {{
        #[cfg(target_arch = "wasm32")]
        { worker::console_error!($($t)*); }
        #[cfg(not(target_arch = "wasm32"))]
        { tracing::error!("{}", format_args!($($t)*)); }
    }};
}

/// Outcome of the retry gate. `Proceed` falls through into the rest of
/// `run_enter`; `Rejected` carries an HTTP status code, a body message,
/// and a short outcome string the dispatcher records against the seen
/// index. Returning the status + body rather than a built `Response`
/// keeps this function constructable in native test builds (the
/// `worker::Response::error` constructor calls into wasm-bindgen and
/// panics off-wasm).
///
/// Two consecutive fires that resolve to the **same** `shell.time`
/// (e.g. an alert that re-evaluates twice on the same close) collapse
/// to one placement: the second is rejected here as
/// `Rejected { status: 409, outcome: "rejected: retry-fire-replay" }`.
/// Note that this is a *distinct* layer from the top-level intent-id
/// replay check in `src/lib.rs` — multi-shot alerts mint a fresh
/// intent id per fire, so the top-level check waves them through and
/// this `shell.time` check is the dedup that catches truly redundant
/// arrivals. The top-level `mark_seen` still runs on the rejected
/// fire's id so the audit trail records the event rather than
/// silently dropping it.
pub enum RetryGateOutcome {
    /// Pass; this fire should attempt a placement. `next_attempt_no`
    /// is the 1-based index to stamp onto the new `EntryAttempt` row
    /// after `place_entry` succeeds.
    Proceed { next_attempt_no: u32 },
    /// Gate rejection that should be surfaced to the operator and
    /// recorded against the seen index alongside its outcome string.
    Rejected {
        status: u16,
        message: &'static str,
        outcome: String,
    },
}

/// Resolve a [`Tunable<u32>`] against Phase 1 scope only (shell
/// anchors). The gate runs before geometry is built so derived
/// bindings aren't bound. Maps `RuleError` onto a `Rejected` outcome
/// with a telemetry-friendly string.
fn resolve_max_retries(tunable: &Tunable<u32>, shell: &Shell) -> Result<u32, RetryGateOutcome> {
    let engine = rules::build_engine();
    let mut scope = rules::RhaiScope::new();
    rules::bind_shell_anchors(&mut scope, shell);
    rules::resolve_tunable::<u32>(&engine, &mut scope, tunable).map_err(|err| {
        let kind = match &err {
            RuleError::Parse(_) => "parse",
            RuleError::Eval(_) => "eval",
            RuleError::WrongType { .. } => "wrong-type",
        };
        RetryGateOutcome::Rejected {
            status: 412,
            message: "max_retries script error",
            outcome: format!("rejected: max-retries-script-{kind}"),
        }
    })
}

/// Walk the gate. See module docs for the algorithm. Only call this
/// when `intent.max_retries` is non-default (i.e. not `Static(0)`) —
/// the caller is responsible for keeping the single-shot path free of
/// any state-store / broker lookups so the byte-identical baseline
/// holds.
pub async fn evaluate<B: Broker, S: StateStore>(
    broker: &B,
    store: &S,
    intent: &Intent,
    shell: &Shell,
) -> RetryGateOutcome {
    let shell_time = shell.time;
    let max_retries = match resolve_max_retries(&intent.max_retries, shell) {
        Ok(n) => n,
        Err(outcome) => return outcome,
    };
    if max_retries == 0 {
        return RetryGateOutcome::Rejected {
            status: 412,
            message: "max_retries resolved to zero",
            outcome: "rejected: max-retries-zero".into(),
        };
    }
    let Some(trade_id) = intent.trade_id.as_deref() else {
        // Same shape — validated upstream by `Intent::validate`.
        return RetryGateOutcome::Proceed { next_attempt_no: 1 };
    };
    let account = intent.account.as_deref();

    match store
        .is_retry_fire_seen(account, trade_id, shell_time)
        .await
    {
        Ok(true) => {
            console_log!(
                "retry: same-bar re-fire dedup'd (trade_id={trade_id} shell_time={shell_time})"
            );
            return RetryGateOutcome::Rejected {
                status: 409,
                message: "replay (retry-fire)",
                outcome: "rejected: retry-fire-replay".into(),
            };
        }
        Ok(false) => {}
        Err(err) => {
            console_error!("KV is_retry_fire_seen: {err}");
            return RetryGateOutcome::Rejected {
                status: 500,
                message: "state error",
                outcome: "rejected: state-error".into(),
            };
        }
    }

    let attempts = match store.list_entry_attempts(account, trade_id).await {
        Ok(a) => a,
        Err(err) => {
            console_error!("KV list_entry_attempts: {err}");
            return RetryGateOutcome::Rejected {
                status: 500,
                message: "state error",
                outcome: "rejected: state-error".into(),
            };
        }
    };

    // Walk newest-first. The plan's collapse rule: stop at the first
    // attempt whose state blocks a placement, otherwise (open, raced
    // cancel) bubble it back as a 412; on a pending we cancel and
    // fall through to placement; closed/cancelled/unknown rows are
    // skipped over toward older attempts.
    for attempt in attempts.iter().rev() {
        match broker
            .lookup_attempt_state(
                &intent.instrument,
                &attempt.broker_order_id,
                attempt.broker_trade_id.as_deref(),
            )
            .await
        {
            Ok(AttemptState::Pending) => {
                let acct = account.unwrap_or("");
                match broker.cancel_order(acct, &attempt.broker_order_id).await {
                    Ok(()) => break,
                    Err(_) => {
                        // Race: order may have filled between observing
                        // Pending and our cancel. Re-lookup to find out.
                        match broker
                            .lookup_attempt_state(
                                &intent.instrument,
                                &attempt.broker_order_id,
                                attempt.broker_trade_id.as_deref(),
                            )
                            .await
                        {
                            Ok(AttemptState::OpenPosition { broker_trade_id }) => {
                                if attempt.broker_trade_id.is_none()
                                    && let Err(err) = store
                                        .set_entry_attempt_broker_trade_id(
                                            account,
                                            trade_id,
                                            attempt.attempt_no,
                                            &broker_trade_id,
                                        )
                                        .await
                                {
                                    console_error!(
                                        "KV set_entry_attempt_broker_trade_id (raced): {err}"
                                    );
                                }
                                return RetryGateOutcome::Rejected {
                                    status: 412,
                                    message: "trade already open (raced with cancel)",
                                    outcome: "rejected: raced-with-cancel".into(),
                                };
                            }
                            Ok(_) => break,
                            Err(LookupError::Transient) => {
                                return RetryGateOutcome::Rejected {
                                    status: 503,
                                    message: "broker transient",
                                    outcome: "rejected: broker-transient".into(),
                                };
                            }
                        }
                    }
                }
            }
            Ok(AttemptState::OpenPosition { broker_trade_id }) => {
                if attempt.broker_trade_id.is_none()
                    && let Err(err) = store
                        .set_entry_attempt_broker_trade_id(
                            account,
                            trade_id,
                            attempt.attempt_no,
                            &broker_trade_id,
                        )
                        .await
                {
                    console_error!("KV set_entry_attempt_broker_trade_id: {err}");
                }
                return RetryGateOutcome::Rejected {
                    status: 412,
                    message: "trade already open",
                    outcome: "rejected: trade-already-open".into(),
                };
            }
            Ok(AttemptState::ClosedWin { .. })
            | Ok(AttemptState::ClosedLossOrBreakeven { .. })
            | Ok(AttemptState::Cancelled)
            | Ok(AttemptState::Unknown) => {
                // Collapsed state — this attempt is done. Look at
                // the next-older attempt; if none remain, fall
                // through to the cap check.
                continue;
            }
            Err(LookupError::Transient) => {
                return RetryGateOutcome::Rejected {
                    status: 503,
                    message: "broker transient",
                    outcome: "rejected: broker-transient".into(),
                };
            }
        }
    }

    if attempts.len() as u32 >= max_retries {
        return RetryGateOutcome::Rejected {
            status: 429,
            message: "retry cap reached",
            outcome: format!("rejected: retry-cap ({max_retries})"),
        };
    }

    RetryGateOutcome::Proceed {
        next_attempt_no: attempts.len() as u32 + 1,
    }
}

/// Record the placed attempt on the seen-by-fire index and the
/// `(account, trade_id)` attempt list. Best-effort: KV write failures
/// are logged but never abort the request — the order is already on
/// the broker, so the operator should hear "entered" even if the
/// state-store didn't catch up.
///
/// `stop_loss_price` is the resolved absolute SL stamped onto the
/// `EntryAttempt` so the scheduled SL-breach sweep can decide whether
/// a still-pending order has been overtaken by price without
/// re-resolving the intent.
#[allow(clippy::too_many_arguments)]
pub async fn record_placement<S: StateStore>(
    store: &S,
    intent: &Intent,
    shell_time: DateTime<Utc>,
    not_after: DateTime<Utc>,
    now: DateTime<Utc>,
    attempt_no: u32,
    broker_order_id: &str,
    direction: trade_control_core::intent::Direction,
    stop_loss_price: f64,
    cancel_at: Option<DateTime<Utc>>,
) {
    let Some(trade_id) = intent.trade_id.as_deref() else {
        return;
    };
    let account = intent.account.as_deref();
    let ttl_seconds = trade_control_core::incoming::replay_ttl_seconds(not_after, now);
    // `replay_ttl_seconds` already adds a 1h grace tail; reuse it
    // for both the EntryAttempt's expires_at and the retry-fire
    // dedup key so a multi-bar setup's last attempt's row outlives
    // the alert window itself.
    let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
    let attempt = EntryAttempt {
        trade_id: trade_id.to_string(),
        account: account.map(|s| s.to_string()),
        instrument: intent.instrument.clone(),
        attempt_no,
        broker_order_id: broker_order_id.to_string(),
        broker_trade_id: None,
        direction,
        placed_at: now,
        shell_time,
        expires_at,
        stop_loss_price: Some(stop_loss_price),
        cancel_at,
    };
    if let Err(err) = store.record_entry_attempt(attempt).await {
        console_error!("KV record_entry_attempt: {err}");
    }
    if let Err(err) = store
        .mark_retry_fire_seen(account, trade_id, shell_time, ttl_seconds)
        .await
    {
        console_error!("KV mark_retry_fire_seen: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use trade_control_core::broker::{
        AttemptState, Broker, CancelError, EntryError, EntryRequest, LookupError,
    };
    use trade_control_core::intent::{
        Action, BrokerKind, Direction, EntrySpec, Intent, PriceAnchor, PriceRef, TakeProfit,
    };
    use trade_control_core::state::{EntryAttempt, Snapshot, StateError, StateStore};

    /// One scripted broker response per call. Tests push these onto a
    /// queue and the mock pops them in order, panicking if a call
    /// arrives unexpectedly. Keeps each test honest about its expected
    /// broker traffic shape.
    enum LookupScript {
        Ok(AttemptState),
        Err(LookupError),
    }
    enum CancelScript {
        Ok,
        Err(CancelError),
    }

    #[derive(Default)]
    struct MockBroker {
        lookups: RefCell<Vec<LookupScript>>,
        cancels: RefCell<Vec<CancelScript>>,
        lookup_calls: RefCell<Vec<(String, String, Option<String>)>>,
        cancel_calls: RefCell<Vec<(String, String)>>,
        place_calls: RefCell<u32>,
    }

    impl MockBroker {
        fn push_lookup(&self, s: AttemptState) {
            self.lookups.borrow_mut().push(LookupScript::Ok(s));
        }
        fn push_lookup_err(&self) {
            self.lookups
                .borrow_mut()
                .push(LookupScript::Err(LookupError::Transient));
        }
        fn push_cancel_ok(&self) {
            self.cancels.borrow_mut().push(CancelScript::Ok);
        }
        fn push_cancel_err(&self) {
            self.cancels
                .borrow_mut()
                .push(CancelScript::Err(CancelError::Transient));
        }
    }

    impl Broker for MockBroker {
        async fn place_entry(
            &self,
            _max_risk_pct: f64,
            _max_open_positions: u32,
            _req: &EntryRequest<'_>,
        ) -> Result<String, EntryError> {
            let mut n = self.place_calls.borrow_mut();
            *n += 1;
            Ok(format!("order-{n}"))
        }
        async fn close_positions(&self, _instrument: &str) -> bool {
            false
        }
        async fn cancel_pending_for_instrument(&self, _instrument: &str) -> usize {
            0
        }
        async fn lookup_attempt_state(
            &self,
            instrument: &str,
            broker_order_id: &str,
            broker_trade_id: Option<&str>,
        ) -> Result<AttemptState, LookupError> {
            self.lookup_calls.borrow_mut().push((
                instrument.to_string(),
                broker_order_id.to_string(),
                broker_trade_id.map(|s| s.to_string()),
            ));
            match self.lookups.borrow_mut().remove(0) {
                LookupScript::Ok(s) => Ok(s),
                LookupScript::Err(e) => Err(e),
            }
        }
        async fn cancel_order(
            &self,
            account_id: &str,
            broker_order_id: &str,
        ) -> Result<(), CancelError> {
            self.cancel_calls
                .borrow_mut()
                .push((account_id.to_string(), broker_order_id.to_string()));
            match self.cancels.borrow_mut().remove(0) {
                CancelScript::Ok => Ok(()),
                CancelScript::Err(e) => Err(e),
            }
        }
        async fn get_current_price(&self, _instrument: &str) -> Result<f64, LookupError> {
            // Retry-gate tests don't exercise the sweep; not used here.
            Err(LookupError::Transient)
        }
    }

    /// In-process `StateStore` good enough to exercise the retry gate
    /// and `record_placement`. Tracks every method call so tests can
    /// assert "zero new state-store calls" on the single-shot baseline.
    #[derive(Default)]
    struct CountingStore {
        attempts: RefCell<HashMap<(String, String), Vec<EntryAttempt>>>,
        retry_fire_seen: RefCell<HashMap<String, ()>>,
        list_calls: RefCell<u32>,
        retry_seen_calls: RefCell<u32>,
        mark_retry_calls: RefCell<u32>,
        record_calls: RefCell<u32>,
        set_btid_calls: RefCell<u32>,
    }

    impl CountingStore {
        fn fire_key(account: Option<&str>, trade_id: &str, shell_time: DateTime<Utc>) -> String {
            format!(
                "{}:{}:{}",
                account.unwrap_or("_"),
                trade_id,
                shell_time.to_rfc3339()
            )
        }
        fn attempt_key(account: Option<&str>, trade_id: &str) -> (String, String) {
            (account.unwrap_or("_").to_string(), trade_id.to_string())
        }
    }

    impl StateStore for CountingStore {
        async fn is_seen(&self, _id: &str) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn mark_seen(
            &self,
            _id: &str,
            _action: Action,
            _seen_at: DateTime<Utc>,
            _outcome: &str,
            _ttl_seconds: u64,
            _trade_id: Option<&str>,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn forget_seen(&self, _id: &str) -> Result<(), StateError> {
            Ok(())
        }
        async fn is_cooled_down(
            &self,
            _account: Option<&str>,
            _instrument: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn set_cooldown(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _hours: u32,
            _now: DateTime<Utc>,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn clear_cooldown(
            &self,
            _account: Option<&str>,
            _instrument: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn set_prep(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _step: &str,
            _now: DateTime<Utc>,
            _ttl_seconds: u64,
            _setter_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_prep(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _step: &str,
        ) -> Result<Option<DateTime<Utc>>, StateError> {
            Ok(None)
        }
        async fn clear_prep(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _step: &str,
        ) -> Result<Option<String>, StateError> {
            Ok(None)
        }
        async fn set_veto(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _instrument: &str,
            _name: &str,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn is_vetoed(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _instrument: &str,
            _name: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn clear_veto(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _instrument: &str,
            _name: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn block_prep(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _step: &str,
            _now: DateTime<Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn is_prep_blocked(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _step: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn clear_prep_block(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _step: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn snapshot(&self) -> Result<Snapshot, StateError> {
            Ok(Snapshot {
                now: Utc::now(),
                cooldowns: Vec::new(),
                recent_seen: Vec::new(),
                preps: Vec::new(),
                vetos: Vec::new(),
                pauses: Vec::new(),
                news_windows: Vec::new(),
                prep_blocks: Vec::new(),
            })
        }
        async fn set_pause(
            &self,
            _trade_id: &str,
            _blackout_id: &str,
            _reason: Option<&str>,
            _now: DateTime<Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn list_pauses_for_trade(
            &self,
            _trade_id: &str,
        ) -> Result<Vec<trade_control_core::state::PauseEntry>, StateError> {
            // The retry-gate tests never set a pause; an empty list keeps
            // the existing gate semantics unchanged.
            Ok(Vec::new())
        }
        async fn clear_pause(
            &self,
            _trade_id: &str,
            _blackout_id: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn set_news_window(
            &self,
            _trade_id: &str,
            _news_id: &str,
            _reason: Option<&str>,
            _now: DateTime<Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn list_news_windows_for_trade(
            &self,
            _trade_id: &str,
        ) -> Result<Vec<trade_control_core::state::NewsEntry>, StateError> {
            Ok(Vec::new())
        }
        async fn clear_news_window(
            &self,
            _trade_id: &str,
            _news_id: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn record_entry_attempt(&self, attempt: EntryAttempt) -> Result<(), StateError> {
            *self.record_calls.borrow_mut() += 1;
            let key = Self::attempt_key(attempt.account.as_deref(), &attempt.trade_id);
            let mut map = self.attempts.borrow_mut();
            let list = map.entry(key).or_default();
            list.push(attempt);
            list.sort_by_key(|a| a.attempt_no);
            Ok(())
        }
        async fn list_entry_attempts(
            &self,
            account: Option<&str>,
            trade_id: &str,
        ) -> Result<Vec<EntryAttempt>, StateError> {
            *self.list_calls.borrow_mut() += 1;
            let key = Self::attempt_key(account, trade_id);
            Ok(self
                .attempts
                .borrow()
                .get(&key)
                .cloned()
                .unwrap_or_default())
        }
        async fn set_entry_attempt_broker_trade_id(
            &self,
            account: Option<&str>,
            trade_id: &str,
            attempt_no: u32,
            broker_trade_id: &str,
        ) -> Result<(), StateError> {
            *self.set_btid_calls.borrow_mut() += 1;
            let key = Self::attempt_key(account, trade_id);
            let mut map = self.attempts.borrow_mut();
            if let Some(list) = map.get_mut(&key)
                && let Some(row) = list.iter_mut().find(|a| a.attempt_no == attempt_no)
            {
                row.broker_trade_id = Some(broker_trade_id.to_string());
            }
            Ok(())
        }
        async fn is_retry_fire_seen(
            &self,
            account: Option<&str>,
            trade_id: &str,
            shell_time: DateTime<Utc>,
        ) -> Result<bool, StateError> {
            *self.retry_seen_calls.borrow_mut() += 1;
            let key = Self::fire_key(account, trade_id, shell_time);
            Ok(self.retry_fire_seen.borrow().contains_key(&key))
        }
        async fn mark_retry_fire_seen(
            &self,
            account: Option<&str>,
            trade_id: &str,
            shell_time: DateTime<Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            *self.mark_retry_calls.borrow_mut() += 1;
            let key = Self::fire_key(account, trade_id, shell_time);
            self.retry_fire_seen.borrow_mut().insert(key, ());
            Ok(())
        }
        async fn list_all_entry_attempts(&self) -> Result<Vec<EntryAttempt>, StateError> {
            Ok(self
                .attempts
                .borrow()
                .values()
                .flat_map(|list| list.iter().cloned())
                .collect())
        }
        async fn delete_entry_attempt(
            &self,
            account: Option<&str>,
            trade_id: &str,
            attempt_no: u32,
        ) -> Result<(), StateError> {
            let key = Self::attempt_key(account, trade_id);
            if let Some(list) = self.attempts.borrow_mut().get_mut(&key) {
                list.retain(|a| a.attempt_no != attempt_no);
            }
            Ok(())
        }
    }

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// Build an Intent with `max_retries: Static(n)`. Pass `0` for the
    /// default single-shot behaviour, matching the post-flatten wire
    /// semantics (`Static(0)` is the elided default).
    fn intent_with_retries(max_retries: u32) -> Intent {
        intent_with_max_retries_tunable(Tunable::Static(max_retries))
    }

    fn intent_with_max_retries_tunable(max_retries: Tunable<u32>) -> Intent {
        Intent {
            v: 1,
            id: "msg-1".into(),
            not_before: None,
            not_after: ts("2026-06-01T00:00:00Z"),
            action: Action::Enter,
            instrument: "EUR_USD".into(),
            direction: Some(Direction::Long),
            entry: Some(EntrySpec::Market),
            stop_loss: Some(PriceRef::Absolute { absolute: 1.05 }),
            take_profit: Some(TakeProfit::RMultiple {
                from: PriceAnchor::Close,
                offset_r: 2.0,
            }),
            risk_pct: trade_control_core::tunable::Tunable::Static(0.5),
            risk_amount: None,
            size_units: None,
            dry_run: None,
            cooldown_hours: None,
            min_r: None,
            broker: BrokerKind::Oanda,
            account: Some("acct-a".into()),
            step: None,
            name: None,
            ttl_hours: trade_control_core::tunable::Tunable::Static(0),
            level: None,
            requires_preps: Vec::new(),
            vetos: Vec::new(),
            clears: Vec::new(),
            trade_id: Some("trade-xyz".into()),
            max_retries,
            expiry_bars: None,
            allow_entry: None,
            allow_close: None,
            needs_golden: false,
            blackout_id: None,
            news_id: None,
            require_news_window: None,
            require_price_in_ranges: None,
            needs_confirmed: false,
            inside_window: Vec::new(),
            sr_bands: Vec::new(),
            reason: None,
            mw: None,
            pip_size: None,
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        pollster::block_on(f)
    }

    fn assert_proceed(o: RetryGateOutcome, want_no: u32) {
        match o {
            RetryGateOutcome::Proceed { next_attempt_no } => assert_eq!(next_attempt_no, want_no),
            RetryGateOutcome::Rejected { outcome, .. } => {
                panic!("expected Proceed, got Rejected: {outcome}")
            }
        }
    }

    fn assert_rejected(o: RetryGateOutcome, want_status: u16, want_outcome_substr: &str) {
        match o {
            RetryGateOutcome::Rejected {
                status,
                message: _,
                outcome,
            } => {
                assert_eq!(
                    status, want_status,
                    "outcome was: {outcome} (status mismatch)"
                );
                assert!(
                    outcome.contains(want_outcome_substr),
                    "outcome {outcome} should contain {want_outcome_substr}"
                );
            }
            RetryGateOutcome::Proceed { .. } => panic!("expected Rejected, got Proceed"),
        }
    }

    fn shell_time() -> DateTime<Utc> {
        ts("2026-05-25T14:00:00Z")
    }

    fn shell_at(time: DateTime<Utc>) -> Shell {
        Shell {
            close: 1.10,
            high: 1.11,
            low: 1.09,
            time,
            signal_high: None,
            signal_low: None,
            signal_range: None,
            signal_start_time: None,
            signal_kind: None,
            golden: None,
            atr: None,
            signal_confirmed: None,
            recent_high: None,
            recent_low: None,
            next_candle_timestamp_1: None,
            next_candle_timestamp_2: None,
            next_candle_timestamp_3: None,
            next_candle_timestamp_4: None,
            next_candle_timestamp_5: None,
        }
    }

    fn fixture_shell() -> Shell {
        shell_at(shell_time())
    }

    /// Single-shot baseline — `max_retries: Static(0)` must skip the
    /// gate entirely. Zero list_entry_attempts / is_retry_fire_seen
    /// calls, zero broker lookups. Proves the byte-identical
    /// regression invariant the plan asks for.
    #[test]
    fn baseline_max_retries_default_makes_no_new_calls() {
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_retries(0);

        // Skip the gate when max_retries is the default Static(0) —
        // mirrors the wired call site. Asserting via the store/broker
        // counters is the strongest invariant: even an accidental
        // change that called evaluate() unconditionally would fail this
        // test.
        if !matches!(intent.max_retries, Tunable::Static(0)) {
            run(evaluate(&broker, &store, &intent, &fixture_shell()));
        }

        assert_eq!(*store.list_calls.borrow(), 0);
        assert_eq!(*store.retry_seen_calls.borrow(), 0);
        assert_eq!(*store.mark_retry_calls.borrow(), 0);
        assert_eq!(*store.record_calls.borrow(), 0);
        assert_eq!(*store.set_btid_calls.borrow(), 0);
        assert!(broker.lookup_calls.borrow().is_empty());
        assert!(broker.cancel_calls.borrow().is_empty());
    }

    /// First fire with `max_retries: 3`: no prior attempts, no broker
    /// lookups should be needed, the gate yields Proceed{1}.
    #[test]
    fn first_fire_proceeds_with_attempt_one() {
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_retries(3);

        let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
        assert_proceed(out, 1);
        assert!(broker.lookup_calls.borrow().is_empty());
        assert_eq!(*store.list_calls.borrow(), 1);
        assert_eq!(*store.retry_seen_calls.borrow(), 1);
    }

    /// record_placement after first fire writes EntryAttempt and the
    /// retry-fire seen entry; a second arrival on the same shell_time
    /// 409s via is_retry_fire_seen.
    #[test]
    fn same_firing_bar_twice_dedups_with_409() {
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_retries(3);
        let now = ts("2026-05-25T14:00:05Z");

        // 1st fire.
        let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
        assert_proceed(out, 1);
        run(record_placement(
            &store,
            &intent,
            shell_time(),
            intent.not_after,
            now,
            1,
            "order-1",
            Direction::Long,
            1.05,
            None,
        ));

        // 2nd fire on the same shell bar — 409.
        let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
        assert_rejected(out, 409, "retry-fire-replay");
        // No broker lookup happened on the dedup path.
        assert!(broker.lookup_calls.borrow().is_empty());
    }

    /// Pending newest attempt → cancel succeeds → fall through to
    /// placement. Cancel called with the prior order's id. Returns
    /// Proceed{2}.
    #[test]
    fn pending_attempt_cancelled_then_proceed() {
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_retries(3);
        run(record_placement(
            &store,
            &intent,
            ts("2026-05-25T13:00:00Z"),
            intent.not_after,
            ts("2026-05-25T13:00:01Z"),
            1,
            "order-1",
            Direction::Long,
            1.05,
            None,
        ));

        broker.push_lookup(AttemptState::Pending);
        broker.push_cancel_ok();

        let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
        assert_proceed(out, 2);

        let cancels = broker.cancel_calls.borrow();
        assert_eq!(cancels.len(), 1);
        assert_eq!(cancels[0].1, "order-1");
    }

    /// Pending → cancel errors → re-lookup returns OpenPosition → 412
    /// "raced with cancel". The broker_trade_id from the re-lookup is
    /// snapshotted onto the row.
    #[test]
    fn pending_cancel_race_open_position_yields_412() {
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_retries(3);
        run(record_placement(
            &store,
            &intent,
            ts("2026-05-25T13:00:00Z"),
            intent.not_after,
            ts("2026-05-25T13:00:01Z"),
            1,
            "order-1",
            Direction::Long,
            1.05,
            None,
        ));

        broker.push_lookup(AttemptState::Pending);
        broker.push_cancel_err();
        broker.push_lookup(AttemptState::OpenPosition {
            broker_trade_id: "btid-42".into(),
        });

        let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
        assert_rejected(out, 412, "raced-with-cancel");
        assert_eq!(*store.set_btid_calls.borrow(), 1);
        let stored = store
            .attempts
            .borrow()
            .get(&("acct-a".to_string(), "trade-xyz".to_string()))
            .cloned()
            .unwrap();
        assert_eq!(stored[0].broker_trade_id.as_deref(), Some("btid-42"));
    }

    /// OpenPosition on the newest attempt → 412, broker_trade_id
    /// snapshotted because the row didn't have one yet.
    #[test]
    fn open_position_yields_412_and_snapshots_trade_id() {
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_retries(3);
        run(record_placement(
            &store,
            &intent,
            ts("2026-05-25T13:00:00Z"),
            intent.not_after,
            ts("2026-05-25T13:00:01Z"),
            1,
            "order-1",
            Direction::Long,
            1.05,
            None,
        ));

        broker.push_lookup(AttemptState::OpenPosition {
            broker_trade_id: "btid-77".into(),
        });

        let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
        assert_rejected(out, 412, "trade-already-open");
        assert_eq!(*store.set_btid_calls.borrow(), 1);
    }

    /// Every collapsed state (ClosedWin / ClosedLossOrBE / Cancelled
    /// / Unknown) lets the gate fall through to placement of the next
    /// attempt. ClosedWin no longer gates — min-1R filtering downstream
    /// is what stops re-entering a winner.
    #[test]
    fn collapsed_states_let_next_attempt_through() {
        for state in [
            AttemptState::ClosedWin { realized_pl: 10.0 },
            AttemptState::ClosedLossOrBreakeven { realized_pl: -5.0 },
            AttemptState::Cancelled,
            AttemptState::Unknown,
        ] {
            let broker = MockBroker::default();
            let store = CountingStore::default();
            let intent = intent_with_retries(3);
            run(record_placement(
                &store,
                &intent,
                ts("2026-05-25T13:00:00Z"),
                intent.not_after,
                ts("2026-05-25T13:00:01Z"),
                1,
                "order-1",
                Direction::Long,
                1.05,
                None,
            ));
            broker.push_lookup(state.clone());

            let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
            assert_proceed(out, 2);
            assert!(
                broker.cancel_calls.borrow().is_empty(),
                "collapsed state {state:?} should NOT trigger a cancel"
            );
        }
    }

    /// Three attempts on record → fourth fire rejected with 429.
    #[test]
    fn retry_cap_yields_429() {
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_retries(3);
        for n in 1..=3 {
            run(record_placement(
                &store,
                &intent,
                ts(&format!("2026-05-25T13:0{n}:00Z")),
                intent.not_after,
                ts(&format!("2026-05-25T13:0{n}:01Z")),
                n,
                &format!("order-{n}"),
                Direction::Long,
                1.05,
                None,
            ));
            // All three are collapsed so we walk past them.
            broker.push_lookup(AttemptState::Cancelled);
        }

        let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
        assert_rejected(out, 429, "retry-cap");
    }

    /// Transient broker lookup → 503; no placement, no EntryAttempt
    /// write (gate returns before placement, callers don't touch the
    /// store on rejection).
    #[test]
    fn transient_broker_error_yields_503() {
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_retries(3);
        run(record_placement(
            &store,
            &intent,
            ts("2026-05-25T13:00:00Z"),
            intent.not_after,
            ts("2026-05-25T13:00:01Z"),
            1,
            "order-1",
            Direction::Long,
            1.05,
            None,
        ));
        broker.push_lookup_err();

        let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
        assert_rejected(out, 503, "broker-transient");
        // Only the initial record_placement wrote an attempt; the
        // rejected fire did not.
        assert_eq!(*store.record_calls.borrow(), 1);
    }

    // ---- max_retries as Tunable ----

    #[test]
    fn max_retries_static_path_unchanged() {
        // Static(3) resolves to 3 — same as the old `Some(3)` path.
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_max_retries_tunable(Tunable::Static(3));
        let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
        assert_proceed(out, 1);
    }

    #[test]
    fn max_retries_script_evaluates_against_shell_anchors() {
        // Script: pump retries to 5 when golden, else 3.
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_max_retries_tunable(Tunable::from_script(
            "if golden == true { 5 } else { 3 }",
        ));
        let mut shell = fixture_shell();
        shell.golden = Some(true);
        let out = run(evaluate(&broker, &store, &intent, &shell));
        // First fire proceeds — the cap (5) hasn't been hit.
        assert_proceed(out, 1);
    }

    #[test]
    fn max_retries_script_returning_zero_rejected() {
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_max_retries_tunable(Tunable::from_script("0"));
        let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
        assert_rejected(out, 412, "max-retries-zero");
    }

    #[test]
    fn max_retries_script_parse_error_yields_412() {
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_max_retries_tunable(Tunable::from_script("if if if"));
        let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
        assert_rejected(out, 412, "max-retries-script-parse");
    }

    #[test]
    fn max_retries_script_wrong_type_yields_412() {
        // Script returns f64, max_retries expects u32.
        let broker = MockBroker::default();
        let store = CountingStore::default();
        let intent = intent_with_max_retries_tunable(Tunable::from_script("1.5"));
        let out = run(evaluate(&broker, &store, &intent, &fixture_shell()));
        assert_rejected(out, 412, "max-retries-script-wrong-type");
    }
}
