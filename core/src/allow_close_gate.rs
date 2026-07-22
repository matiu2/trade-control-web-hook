//! The `allow_close` gate.
//!
//! Symmetric with the entry-side `allow_entry` gate but for the
//! consolidated close-on-reversal path. Reads the intent's optional
//! `allow_close: Tunable<bool>` and runs it against the **shell-anchor
//! scope only** (no `Resolved` — closes don't compute SL/TP, so there's
//! no derived geometry to bind). Returning `false` is a 412; a script
//! error (parse / eval / wrong type) is also a 412 with a more
//! specific outcome string in the rejection log.
//!
//! Lives in `core` so the live worker (`run_close` in `src/lib.rs`) and
//! the offline replay share one implementation and can't drift — see
//! `[[strategy_changes_in_both_replayer_and_worker]]`. A blocked
//! `allow_close` keeps a position OPEN live; without this the replay would
//! close it and silently diverge.
//!
//! The candle-quality checks (`needs_golden` / `needs_confirmed`) run
//! ahead of the script. The worker's `candle_gate` is the production
//! single-source-of-truth for those two checks on the entry path; the
//! identical two-field predicate is inlined here ([`candle_quality`]) so
//! this gate can live in `core` without depending on the worker-only
//! `candle_gate` module. Both implementations are the same trivial
//! `Some(true)`-required check and must stay in lockstep.

use crate::intent::{Intent, Shell};
use crate::rules::{self, RuleError};

/// Outcome of the allow_close gate. Maps onto a 412 rejection at the
/// dispatch layer.
#[derive(Debug, PartialEq, Eq)]
pub enum AllowCloseOutcome {
    /// No gate configured, or the gate evaluated to true. Continue.
    Proceed,
    /// `allow_close` returned `false`. 412 "close blocked".
    Blocked,
    /// `needs_golden` was set but the shell did not carry
    /// `golden: Some(true)`. Distinct from `Blocked` so the rejection
    /// log makes the cause obvious.
    NeedsGoldenUnmet,
    /// `needs_confirmed` was set but the shell did not carry
    /// `signal_confirmed: Some(true)`.
    NeedsConfirmedUnmet,
    /// `allow_close` script error. 412 "close blocked: script error".
    ScriptError {
        /// Short label for the rejection-outcome telemetry string
        /// (`parse` / `eval` / `wrong-type`).
        kind: &'static str,
        /// Display string for the worker's `rlog_err!` log.
        message: String,
    },
}

/// The candle-quality slice of the gate, mirroring the worker's
/// `candle_gate::evaluate`: `needs_golden` / `needs_confirmed` each
/// require the matching shell field to be `Some(true)`. `Some(false)` and
/// `None` both reject (conservative). `golden` is checked before
/// `confirmed` so the outcome names the stricter check first when both
/// fail. Returns `None` when both checks pass.
fn candle_quality(intent: &Intent, shell: &Shell) -> Option<AllowCloseOutcome> {
    if intent.needs_golden && shell.golden != Some(true) {
        return Some(AllowCloseOutcome::NeedsGoldenUnmet);
    }
    if intent.needs_confirmed && shell.signal_confirmed != Some(true) {
        return Some(AllowCloseOutcome::NeedsConfirmedUnmet);
    }
    None
}

/// Run the gate. Caller logs / maps the outcome to an HTTP response.
///
/// Order: candle-quality checks first (cheap, no Rhai), then the
/// `allow_close` script. AND semantics — every set gate must pass.
pub fn evaluate(intent: &Intent, shell: &Shell) -> AllowCloseOutcome {
    if let Some(unmet) = candle_quality(intent, shell) {
        return unmet;
    }
    let Some(gate) = &intent.allow_close else {
        return AllowCloseOutcome::Proceed;
    };
    let engine = rules::build_engine();
    let mut scope = rules::RhaiScope::new();
    rules::bind_shell_anchors(&mut scope, shell);
    match rules::resolve_tunable::<bool>(&engine, &mut scope, gate) {
        Ok(true) => AllowCloseOutcome::Proceed,
        Ok(false) => AllowCloseOutcome::Blocked,
        Err(err) => {
            let kind = match &err {
                RuleError::Parse(_) => "parse",
                RuleError::Eval(_) => "eval",
                RuleError::WrongType { .. } => "wrong-type",
            };
            AllowCloseOutcome::ScriptError {
                kind,
                message: err.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::{Action, BrokerKind};
    use crate::tunable::Tunable;
    use chrono::{DateTime, Utc};

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn shell_with(golden: Option<bool>, confirmed: Option<bool>) -> Shell {
        Shell {
            close: 1.1000,
            high: 1.1020,
            low: 1.0980,
            open: None,
            time: ts("2026-05-26T10:00:00Z"),
            signal_high: Some(1.1018),
            signal_low: Some(1.0982),
            signal_range: Some(0.0036),
            signal_start_time: Some(ts("2026-05-26T09:00:00Z")),
            signal_kind: None,
            band_anchor: None,
            golden,
            atr: None,
            signal_confirmed: confirmed,
            recent_high: None,
            recent_low: None,
            next_candle_timestamp_1: None,
            next_candle_timestamp_2: None,
            next_candle_timestamp_3: None,
            next_candle_timestamp_4: None,
            next_candle_timestamp_5: None,
        }
    }

    fn close_intent(
        needs_golden: bool,
        needs_confirmed: bool,
        allow_close: Option<Tunable<bool>>,
    ) -> Intent {
        Intent {
            entry_level_vetos: Vec::new(),
            v: 1,
            id: "msg-1".into(),
            not_before: None,
            not_after: ts("2026-06-01T00:00:00Z"),
            action: Action::Close,
            instrument: "EUR_USD".into(),
            direction: None,
            entry: None,
            stop_loss: None,
            take_profit: None,
            risk_pct: Tunable::Static(1.0),
            risk_amount: None,
            size_units: None,
            dry_run: None,
            cooldown_hours: None,
            min_r: None,
            broker: BrokerKind::Oanda,
            account: None,
            step: None,
            name: None,
            ttl_hours: Tunable::Static(0),
            level: None,
            requires_preps: Vec::new(),
            vetos: Vec::new(),
            clears: Vec::new(),
            trade_id: Some("t-1".into()),
            max_retries: Tunable::Static(0),
            expiry_bars: None,
            allow_entry: None,
            allow_close,
            needs_golden,
            blackout_id: None,
            news_id: None,
            require_news_window: None,
            require_price_in_ranges: None,
            needs_confirmed,
            inside_window: Vec::new(),
            sr_bands: Vec::new(),
            veto_on_reversal: false,
            reason: None,
            mw: None,
            pip_size: None,
            tick_size: None,
            spread_window: None,
            trade_plan: None,
            blackout_close: crate::intent::BlackoutCloseAction::default(),
            breakeven: None,
            include_archived: false,
        }
    }

    #[test]
    fn no_gate_no_candle_checks_proceeds() {
        let outcome = evaluate(&close_intent(false, false, None), &shell_with(None, None));
        assert_eq!(outcome, AllowCloseOutcome::Proceed);
    }

    #[test]
    fn needs_golden_blocks_close_when_shell_missing_golden() {
        let outcome = evaluate(&close_intent(true, false, None), &shell_with(None, None));
        assert_eq!(outcome, AllowCloseOutcome::NeedsGoldenUnmet);
    }

    #[test]
    fn needs_confirmed_blocks_close_when_shell_unconfirmed() {
        let outcome = evaluate(
            &close_intent(false, true, None),
            &shell_with(None, Some(false)),
        );
        assert_eq!(outcome, AllowCloseOutcome::NeedsConfirmedUnmet);
    }

    #[test]
    fn allow_close_static_false_blocks() {
        let outcome = evaluate(
            &close_intent(false, false, Some(Tunable::Static(false))),
            &shell_with(None, None),
        );
        assert_eq!(outcome, AllowCloseOutcome::Blocked);
    }

    #[test]
    fn allow_close_static_true_proceeds() {
        let outcome = evaluate(
            &close_intent(false, false, Some(Tunable::Static(true))),
            &shell_with(None, None),
        );
        assert_eq!(outcome, AllowCloseOutcome::Proceed);
    }

    #[test]
    fn allow_close_script_reads_shell_anchors() {
        // signal_confirmed binding lives in bind_shell_anchors; this
        // proves the close gate scope is wired up the same way the
        // entry gate is.
        let gate = Tunable::from_script("signal_confirmed == true");
        let proceed = evaluate(
            &close_intent(false, false, Some(gate.clone())),
            &shell_with(None, Some(true)),
        );
        assert_eq!(proceed, AllowCloseOutcome::Proceed);

        let blocked = evaluate(
            &close_intent(false, false, Some(gate)),
            &shell_with(None, Some(false)),
        );
        assert_eq!(blocked, AllowCloseOutcome::Blocked);
    }

    #[test]
    fn allow_close_compose_and_with_candle_gate() {
        // Both gates set, both pass → Proceed.
        let gate_true = Tunable::from_script("signal_confirmed == true");
        let outcome = evaluate(
            &close_intent(true, false, Some(gate_true.clone())),
            &shell_with(Some(true), Some(true)),
        );
        assert_eq!(outcome, AllowCloseOutcome::Proceed);

        // needs_golden fails first, even if the script would also pass.
        let outcome = evaluate(
            &close_intent(true, false, Some(gate_true)),
            &shell_with(None, Some(true)),
        );
        assert_eq!(outcome, AllowCloseOutcome::NeedsGoldenUnmet);

        // Candle passes, script blocks → Blocked.
        let gate_false = Tunable::from_script("signal_confirmed == false");
        let outcome = evaluate(
            &close_intent(true, false, Some(gate_false)),
            &shell_with(Some(true), Some(true)),
        );
        assert_eq!(outcome, AllowCloseOutcome::Blocked);
    }

    #[test]
    fn allow_close_parse_error_surfaces_kind() {
        let gate = Tunable::from_script("if if if");
        match evaluate(
            &close_intent(false, false, Some(gate)),
            &shell_with(None, None),
        ) {
            AllowCloseOutcome::ScriptError { kind, .. } => assert_eq!(kind, "parse"),
            other => panic!("expected ScriptError(parse), got {other:?}"),
        }
    }

    #[test]
    fn allow_close_wrong_return_type_surfaces_kind() {
        let gate = Tunable::from_script("1.5");
        match evaluate(
            &close_intent(false, false, Some(gate)),
            &shell_with(None, None),
        ) {
            AllowCloseOutcome::ScriptError { kind, .. } => assert_eq!(kind, "wrong-type"),
            other => panic!("expected ScriptError(wrong-type), got {other:?}"),
        }
    }
}
