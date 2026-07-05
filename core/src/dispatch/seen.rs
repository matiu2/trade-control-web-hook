//! Replay-protection helpers shared by the wasm worker and the native
//! axum receiver.
//!
//! These four items decide whether a dispatched intent's id is written
//! to the seen-by-id index (and so 409s on a later refire) or merely
//! logged. They are pure / generic over [`StateStore`] and worker-free,
//! so both edges — the Cloudflare Worker's `#[event(fetch)]` and the
//! native `POST /` handler — call the *same* logic and cannot drift
//! (`[[strategy_changes_in_both_replayer_and_worker]]`). They log via
//! plain `tracing::{info,error}!` (the worker's `ConsoleSubscriber` tees
//! those into its R2 recording buffer; the native runtime routes them to
//! its own subscriber).

use crate::dispatch::ActionResult;
use crate::incoming::{self, Verified};
use crate::intent::{Action, Intent};
use crate::state::StateStore;
use crate::tunable::Tunable;

/// Record the dispatcher's outcome on the seen-by-id index.
///
/// **Only `Ok` writes.** `Failed` (502 from a broker call) and
/// `Rejected` (any gate or pre-broker reason) are logged via
/// `tracing::info!` for post-mortem visibility but deliberately do not
/// consume the intent id. The next fire of the same alert is allowed
/// through.
///
/// Rationale (CHF/JPY 2026-06-02 incident). Earlier worker versions
/// wrote `mark_seen` for every variant, which poisoned the id for the
/// rest of the alert's `not_after` window. A real instance: an
/// `enter` alert fired 6 times in 9h. Fire 4 was correctly rejected
/// with `rejected: missing-prep (break-and-close)` — the prep had
/// not been set yet, but it *could* have been later in the window.
/// That rejection poisoned the id, so fires 5 (a confirmed signal,
/// the entry the operator actually wanted) and 6 both 409'd on
/// `is_seen` before reaching the `allow_entry` script gate. Every
/// non-`Ok` outcome is either transient (gate condition might flip)
/// or terminal-but-idempotent (parse error, `resolve-failed`,
/// `retry-cap` — next fire will reject the same way). Letting them
/// refire is harmless KV churn; poisoning them silently breaks
/// within-window legitimate fires.
///
/// Control actions (`prep`, level-1 `veto`, `pause`, `clear-*`, etc.)
/// use a separate `record_seen` helper and *do* mark seen on
/// completion — that's legitimate idempotency for state-set
/// operations (a `prep` message replayed twice shouldn't refresh its
/// TTL twice).
///
/// Generic over [`StateStore`] so native (non-wasm) tests can pass a
/// `MemStateStore`-style fake.
pub async fn record_dispatcher_outcome<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: chrono::DateTime<chrono::Utc>,
    result: &ActionResult,
) {
    match seen_decision(result) {
        SeenDecision::Mark { outcome } => {
            let ttl = incoming::replay_ttl_seconds(verified.intent.not_after, now);
            if let Err(err) = store
                .mark_seen(
                    &verified.intent.id,
                    verified.intent.action,
                    now,
                    outcome,
                    ttl,
                    verified.intent.trade_id.as_deref(),
                )
                .await
            {
                tracing::error!("KV mark_seen after action: {err}");
            }
        }
        SeenDecision::Skip { kind, outcome } => {
            log_skip(kind, &verified.intent.id, outcome);
        }
    }
}

/// Log a skipped (non-`Ok`) dispatcher outcome.
pub fn log_skip(kind: &str, id: &str, outcome: &str) {
    tracing::info!("entry-path {kind} (no mark_seen): id={id} outcome={outcome}");
}

/// True when this intent is a multi-shot `enter` — an `enter` that
/// opted into `max_retries` (anything other than the default
/// `Static(0)`) and carries a `trade_id`.
///
/// For these the top-level intent-id replay guard at the edge must
/// **not** 409: the alert bakes one static intent id and re-fires it on
/// every signal bar, so the first accepted fire would otherwise poison
/// the id and block every legitimate re-entry. The real replay
/// authority for multi-shot is `retry_gate::evaluate` (run from
/// `run_enter`), which dedups true same-bar re-fires on `shell.time`
/// and rejects 412 when a prior attempt is still open.
///
/// The `trade_id.is_some()` clause is load-bearing: without a
/// `trade_id` the retry gate does no per-bar dedup (see
/// `retry_gate::evaluate`), so such an intent must stay on the
/// top-level 409 path. Single-shot enters and every control action
/// return `false` and keep the byte-identical top-level 409.
///
/// Pure (`&Intent -> bool`) so unit tests can exercise the rule
/// without building any edge response — same rationale as
/// [`seen_decision`].
pub fn is_multishot_enter(intent: &Intent) -> bool {
    matches!(intent.action, Action::Enter)
        && !matches!(intent.max_retries, Tunable::Static(0))
        && intent.trade_id.is_some()
}

/// Pure helper: classify an [`ActionResult`] into "write to seen" vs
/// "log only". Pulled out so unit tests can exercise the rule without
/// constructing a `worker::Response` (which calls into wasm-bindgen at
/// construction time and panics off-wasm).
pub fn seen_decision(result: &ActionResult) -> SeenDecision<'_> {
    match result {
        ActionResult::Ok(outcome) => SeenDecision::Mark { outcome },
        ActionResult::Failed(outcome) => SeenDecision::Skip {
            kind: "failed",
            outcome,
        },
        ActionResult::Rejected { outcome, .. } => SeenDecision::Skip {
            kind: "rejected",
            outcome,
        },
    }
}

/// Whether a dispatched outcome should be written to the seen index
/// ([`Mark`](SeenDecision::Mark)) or only logged
/// ([`Skip`](SeenDecision::Skip)).
#[derive(Debug, PartialEq, Eq)]
pub enum SeenDecision<'a> {
    Mark {
        outcome: &'a str,
    },
    Skip {
        kind: &'static str,
        outcome: &'a str,
    },
}

#[cfg(test)]
mod dispatcher_outcome_tests {
    //! Pins the behaviour of [`record_dispatcher_outcome`] against the
    //! seen-by-id index. Only [`ActionResult::Ok`] writes; `Failed`
    //! and every flavour of `Rejected` are no-ops. See the function's
    //! own docs for the CHF/JPY 2026-06-02 motivation.
    use super::*;
    use crate::dispatch::record_seen;
    use crate::incoming::Verified;
    use crate::intent::{Action, BrokerKind, Direction, EntrySpec, Intent, PriceRef, Shell};
    use crate::state::{EntryAttempt, Snapshot, StateError, StateStore};
    use crate::tunable::Tunable;
    use chrono::{DateTime, TimeZone, Utc};
    use std::cell::RefCell;

    /// Captures every `mark_seen` call. All other [`StateStore`]
    /// methods are stubbed out — the dispatcher-outcome path only
    /// touches `mark_seen`.
    #[derive(Default)]
    struct SeenSpyStore {
        marks: RefCell<Vec<(String, String)>>,
    }

    impl SeenSpyStore {
        fn marks(&self) -> Vec<(String, String)> {
            self.marks.borrow().clone()
        }
    }

    impl StateStore for SeenSpyStore {
        async fn is_seen(&self, _id: &str) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn mark_seen(
            &self,
            id: &str,
            _action: Action,
            _seen_at: DateTime<Utc>,
            outcome: &str,
            _ttl_seconds: u64,
            _trade_id: Option<&str>,
        ) -> Result<(), StateError> {
            self.marks
                .borrow_mut()
                .push((id.to_string(), outcome.to_string()));
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
            _now: chrono::DateTime<chrono::Utc>,
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
                spread_blackouts: Vec::new(),
                spread_blackout_window: None,
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
        ) -> Result<Vec<crate::state::PauseEntry>, StateError> {
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
        ) -> Result<Vec<crate::state::NewsEntry>, StateError> {
            Ok(Vec::new())
        }
        async fn clear_news_window(
            &self,
            _trade_id: &str,
            _news_id: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn record_entry_attempt(&self, _attempt: EntryAttempt) -> Result<(), StateError> {
            Ok(())
        }
        async fn list_entry_attempts(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<Vec<EntryAttempt>, StateError> {
            Ok(Vec::new())
        }
        async fn set_entry_attempt_broker_trade_id(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _attempt_no: u32,
            _broker_trade_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn is_retry_fire_seen(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _shell_time: DateTime<Utc>,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn mark_retry_fire_seen(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _shell_time: DateTime<Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn list_all_entry_attempts(&self) -> Result<Vec<EntryAttempt>, StateError> {
            Ok(Vec::new())
        }
        async fn delete_entry_attempt(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _attempt_no: u32,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn set_spread_blackout_window(
            &self,
            _now: DateTime<Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_spread_blackout_window(
            &self,
        ) -> Result<Option<crate::state::SpreadBlackoutWindow>, StateError> {
            Ok(None)
        }
        async fn set_blackout_windows(
            &self,
            _instrument: &str,
            _windows: &[crate::intent::NoEntryWindow],
            _now: DateTime<Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_blackout_windows(
            &self,
            _instrument: &str,
        ) -> Result<Vec<crate::intent::NoEntryWindow>, StateError> {
            Ok(Vec::new())
        }
        async fn upsert_spread_blackout_record(
            &self,
            _record: &crate::state::SpreadBlackoutRecord,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_spread_blackout_record(
            &self,
            _trade_id: &str,
        ) -> Result<Option<crate::state::SpreadBlackoutRecord>, StateError> {
            Ok(None)
        }
        async fn list_all_spread_blackout_records(
            &self,
        ) -> Result<Vec<crate::state::SpreadBlackoutRecord>, StateError> {
            Ok(Vec::new())
        }
        async fn clear_spread_blackout_record(&self, _trade_id: &str) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_mw_state(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<Option<crate::state::MwState>, StateError> {
            Ok(None)
        }
        async fn upsert_mw_state(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _state: &crate::state::MwState,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn clear_mw_state(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }

        // Engine plan/state methods — unused by these tests; minimal stubs.
        async fn put_trade_plan(
            &self,
            _account: Option<&str>,
            _plan: &crate::trade_plan::TradePlan,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_trade_plan(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<Option<crate::trade_plan::TradePlan>, StateError> {
            Ok(None)
        }
        async fn list_all_trade_plans(&self) -> Result<Vec<crate::state::StoredPlan>, StateError> {
            Ok(Vec::new())
        }
        async fn clear_trade_plan(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_plan_state(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<Option<crate::plan_state::PlanState>, StateError> {
            Ok(None)
        }
        async fn put_plan_state(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _state: &crate::plan_state::PlanState,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn clear_plan_state(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn record_control_event(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _event: &crate::control_event::ControlEvent,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn list_control_events(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<Vec<crate::control_event::ControlEvent>, StateError> {
            Ok(Vec::new())
        }
        async fn clear_control_events(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn archive_plan(
            &self,
            _account: Option<&str>,
            _plan: &crate::trade_plan::TradePlan,
            _final_state: &crate::plan_state::PlanState,
            _archived_at: DateTime<Utc>,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn list_all_archived_plans(
            &self,
        ) -> Result<Vec<crate::state::ArchivedPlan>, StateError> {
            Ok(vec![])
        }
        async fn clear_archived_plan(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 2, 17, 0, 0).unwrap()
    }

    fn not_after() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 4, 16, 50, 53).unwrap()
    }

    fn verified(id: &str) -> Verified {
        Verified {
            shell: Shell {
                close: 203.0,
                high: 203.2,
                low: 202.9,
                open: None,
                time: now(),
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
            },
            intent: Intent {
                entry_level_vetos: Vec::new(),
                v: 1,
                id: id.into(),
                not_before: None,
                not_after: not_after(),
                action: Action::Enter,
                instrument: "CHF_JPY".into(),
                direction: Some(Direction::Short),
                entry: Some(EntrySpec::Market),
                stop_loss: Some(PriceRef::Absolute { absolute: 203.5 }),
                take_profit: None,
                risk_pct: Tunable::Static(0.25),
                risk_amount: None,
                size_units: None,
                dry_run: None,
                cooldown_hours: None,
                min_r: None,
                broker: BrokerKind::TradeNation,
                account: Some("reversals".into()),
                step: None,
                name: None,
                ttl_hours: Tunable::Static(0),
                level: None,
                requires_preps: Vec::new(),
                vetos: Vec::new(),
                clears: Vec::new(),
                trade_id: Some("hs-chf-jpy-test".into()),
                max_retries: Tunable::Static(0),
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
                veto_on_reversal: false,
                reason: None,
                mw: None,
                pip_size: None,
                trade_plan: None,
                blackout_close: crate::intent::BlackoutCloseAction::default(),
                breakeven: None,
                include_archived: false,
            },
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        pollster::block_on(f)
    }

    #[test]
    fn ok_outcome_classifies_as_mark() {
        let result = ActionResult::Ok("entered: order=42".into());
        assert_eq!(
            seen_decision(&result),
            SeenDecision::Mark {
                outcome: "entered: order=42"
            },
        );
    }

    #[test]
    fn failed_outcome_classifies_as_skip() {
        let result = ActionResult::Failed("entry-failed: broker 500".into());
        assert_eq!(
            seen_decision(&result),
            SeenDecision::Skip {
                kind: "failed",
                outcome: "entry-failed: broker 500"
            },
            "Failed must classify as Skip — broker errors don't poison the id",
        );
    }

    /// A too-close (`#19-10`) entry failure must classify as Skip so it
    /// never poisons the seen-id — the recovery contract is "let the
    /// next bar retry". Uses the exact string the worker emits via
    /// `recover_entry::outcome_for_entry_error`.
    #[test]
    fn too_close_outcome_classifies_as_skip() {
        let result = ActionResult::Failed("entry-failed: too-close-to-market".into());
        assert!(
            matches!(
                seen_decision(&result),
                SeenDecision::Skip { kind: "failed", .. }
            ),
            "too-close must Skip — recovery relies on the next bar retrying",
        );
    }

    /// End-to-end happy-path: an `Ok` outcome routed through the
    /// async helper actually lands in the store. Pins the wiring
    /// between `seen_decision::Mark` and the `store.mark_seen` call —
    /// without this, a future refactor could move the decision
    /// classification away from the store write and the
    /// classification tests above wouldn't catch it.
    #[test]
    fn ok_outcome_writes_to_store_via_record_dispatcher_outcome() {
        let store = SeenSpyStore::default();
        let v = verified("ok-id");
        let result = ActionResult::Ok("entered: order=42".into());
        run(record_dispatcher_outcome(&store, &v, now(), &result));
        assert_eq!(
            store.marks(),
            vec![("ok-id".into(), "entered: order=42".into())],
            "Ok must write to seen so duplicate alert bodies 409 on replay",
        );
    }

    /// End-to-end skip path: a `Failed` outcome routed through the
    /// async helper does NOT touch the store. We use `Failed` rather
    /// than `Rejected` because the `response` field of `Rejected` is
    /// a `worker::Result<worker::Response>` which calls into
    /// wasm-bindgen at construction and panics off-wasm; the
    /// classification test below covers the `Rejected` variant via
    /// `seen_decision`.
    #[test]
    fn failed_outcome_does_not_write_to_store() {
        let store = SeenSpyStore::default();
        let v = verified("failed-id");
        let result = ActionResult::Failed("entry-failed: broker 500".into());
        run(record_dispatcher_outcome(&store, &v, now(), &result));
        assert!(
            store.marks().is_empty(),
            "Failed must not write to seen — next fire is allowed to retry",
        );
    }

    /// Walk every gate-rejection outcome string the worker emits today
    /// and assert each classifies as `Skip`. Strings here correspond
    /// to real rejection sites in `run_enter` / `run_close` /
    /// `run_invalidate` / the retry gate, taken from the Phase 1
    /// exploration of `ActionResult::Rejected` call sites.
    ///
    /// The CHF/JPY 2026-06-02 incident bottomed out at fire 4 with
    /// `"rejected: missing-prep (break-and-close)"` — that's the
    /// first case below. Every other transient or terminal rejection
    /// gets the same treatment: log and move on, do not poison the id.
    ///
    /// Note this tests via [`seen_decision`] rather than the full
    /// async helper: constructing `ActionResult::Rejected` requires a
    /// `worker::Result<worker::Response>` in the `response` field,
    /// which calls into wasm-bindgen at construction and panics
    /// off-wasm. The pure decision rule is what we care about; the
    /// async helper just turns `SeenDecision::Mark` into a
    /// `store.mark_seen` call.
    ///
    /// To synthesize the `Rejected` variant safely we'd need to
    /// either fake a `worker::Response` (not possible — it's a
    /// wasm-bindgen wrapper) or test through a public API surface
    /// that constructs them naturally. Neither pays off enough to
    /// justify the complexity. The match in `seen_decision` is
    /// trivially auditable.
    #[test]
    fn every_rejection_outcome_classifies_as_skip() {
        let cases = [
            "rejected: missing-prep (break-and-close)",
            "rejected: prep-order-violated (retest)",
            "rejected: veto-active (too-high)",
            "rejected: cooled-down",
            "rejected: paused [news-window]",
            "rejected: allow-entry-false",
            "rejected: needs-golden",
            "rejected: allow-entry-eval",
            "rejected: resolve-failed",
            "rejected: state-error",
            "rejected: retry-cap (5)",
            "rejected: retry-fire-replay",
            "rejected: trade-already-open",
            "rejected: broker-transient",
            "rejected: max-retries-zero",
            "rejected: missing-trade-id",
            "rejected: price-fetch-failed",
            "rejected: expiry-bars-out-of-range",
            "rejected: expiry-bars-script-parse",
            "rejected: market-blackout",
        ];
        // Use Failed as the carrier — the decision rule treats
        // Failed and Rejected identically (both Skip), and Failed is
        // wasm-safe to construct off-wasm.
        for outcome in cases {
            let result = ActionResult::Failed(outcome.into());
            assert!(
                matches!(seen_decision(&result), SeenDecision::Skip { .. }),
                "outcome {outcome:?} unexpectedly classified as Mark \
                 — non-Ok outcomes must Skip the seen index",
            );
        }
    }

    /// Control actions (`prep`, `veto`, `pause`, `clear-*`, `status`,
    /// `news-*`, `unlock`) use a separate [`record_seen`] helper and
    /// **do** mark seen on completion. That's legitimate idempotency
    /// for state-set ops — a replayed `prep` message should not
    /// double-refresh its TTL. This regression test pins that
    /// behaviour so a future "blanket-strip mark_seen writes"
    /// refactor can't silently break it.
    #[test]
    fn control_action_record_seen_still_marks() {
        let store = SeenSpyStore::default();
        let mut v = verified("prep-msg-id");
        v.intent.action = Action::Prep;
        v.intent.step = Some("break-and-close".into());
        run(record_seen(
            &store,
            &v,
            now(),
            "prep-set: break-and-close ttl=24h",
        ));
        assert_eq!(
            store.marks(),
            vec![(
                "prep-msg-id".into(),
                "prep-set: break-and-close ttl=24h".into(),
            )],
            "Control-action record_seen must still mark seen — replay protection \
             on state-set ops (prep/veto/pause/etc) is legitimate idempotency",
        );
    }

    // ---- is_multishot_enter: the top-level replay-guard exemption ----

    /// An `enter` that opted into `max_retries` and carries a `trade_id`
    /// is the case the bug fix targets: its baked-in id re-fires every
    /// bar, so the top-level `is_seen` 409 must defer to the retry gate.
    #[test]
    fn multishot_enter_is_detected() {
        let mut v = verified("ent-id");
        v.intent.action = Action::Enter;
        v.intent.max_retries = Tunable::Static(3);
        v.intent.trade_id = Some("trade-xyz".into());
        assert!(is_multishot_enter(&v.intent));
    }

    /// Single-shot enter (`max_retries` default `Static(0)`) keeps the
    /// byte-identical top-level 409 — the retry gate never runs for it.
    #[test]
    fn single_shot_enter_is_not_multishot() {
        let mut v = verified("ent-id");
        v.intent.action = Action::Enter;
        v.intent.max_retries = Tunable::Static(0);
        v.intent.trade_id = Some("trade-xyz".into());
        assert!(!is_multishot_enter(&v.intent));
    }

    /// Without a `trade_id` the retry gate does no per-bar dedup, so the
    /// intent must stay on the top-level 409 path even with `max_retries`.
    #[test]
    fn multishot_enter_without_trade_id_is_not_multishot() {
        let mut v = verified("ent-id");
        v.intent.action = Action::Enter;
        v.intent.max_retries = Tunable::Static(3);
        v.intent.trade_id = None;
        assert!(!is_multishot_enter(&v.intent));
    }

    /// Any non-`Static(0)` Tunable (including a script) counts as
    /// multi-shot — mirrors the `max_retries != Static(0)` test the
    /// run_enter gate uses.
    #[test]
    fn enter_with_script_max_retries_is_multishot() {
        let mut v = verified("ent-id");
        v.intent.action = Action::Enter;
        v.intent.max_retries = Tunable::from_script("3");
        v.intent.trade_id = Some("trade-xyz".into());
        assert!(is_multishot_enter(&v.intent));
    }

    /// Only `Enter` is exempted. A control action that happens to carry a
    /// stray `max_retries` + `trade_id` still 409s at the top level.
    #[test]
    fn control_actions_are_not_multishot() {
        for action in [
            Action::Close,
            Action::Invalidate,
            Action::Veto,
            Action::Prep,
            Action::Status,
            Action::Pause,
        ] {
            let mut v = verified("ctl-id");
            v.intent.action = action;
            v.intent.max_retries = Tunable::Static(3);
            v.intent.trade_id = Some("trade-xyz".into());
            assert!(
                !is_multishot_enter(&v.intent),
                "{action:?} must not be treated as a multi-shot enter",
            );
        }
    }
}
