//! The `allow_entry` gate.
//!
//! Reads the intent's optional `allow_entry: Tunable<bool>` field and
//! runs the operator's script against a full three-phase scope (shell
//! anchors + resolved geometry). Returning `false` is a 412; a script
//! error (parse / eval / wrong type) is also a 412 with a more
//! specific outcome string in the rejection log.
//!
//! The gate is pulled out of `lib::dispatch_enter` so it stays sync
//! and small (no `worker::Env`, no async, no broker handle), which is
//! both easier to read and easier to test in isolation.

use trade_control_core::intent::{Intent, Resolved, Shell};
use trade_control_core::rules::{self, RuleError};

/// Outcome of the allow_entry gate. Maps 1:1 onto the worker's
/// `ActionResult` after the caller adds the intent id to the rejection
/// log line.
#[derive(Debug, PartialEq, Eq)]
pub enum AllowEntryOutcome {
    /// No gate configured, or the gate evaluated to true. Continue.
    Proceed,
    /// Gate returned `false`. 412 "entry blocked".
    Blocked,
    /// `needs_golden` was set on the intent but the incoming shell did
    /// not carry `golden: Some(true)`. 412 "entry blocked: needs-golden".
    /// Distinct from `Blocked` so the rejection log makes the cause
    /// obvious without the operator having to read a script.
    NeedsGoldenUnmet,
    /// Script error. 412 "entry blocked: script error".
    ScriptError {
        /// Short label for the rejection-outcome telemetry string
        /// (`parse` / `eval` / `wrong-type`). Distinct from the inner
        /// error so the worker doesn't have to re-match.
        kind: &'static str,
        /// Display string for the worker's `console_error!` log.
        message: String,
    },
}

/// Run the gate. Caller logs / maps the outcome to an HTTP response.
///
/// Order: `needs_golden` is checked first (cheap, no scripting), then
/// the `allow_entry` script. Both gates must pass — semantics are AND.
pub fn evaluate(
    intent: &Intent,
    shell: &Shell,
    resolved: &Resolved,
    pip_size: f64,
) -> AllowEntryOutcome {
    if intent.needs_golden && shell.golden != Some(true) {
        return AllowEntryOutcome::NeedsGoldenUnmet;
    }
    let Some(gate) = &intent.allow_entry else {
        return AllowEntryOutcome::Proceed;
    };
    let engine = rules::build_engine();
    let mut scope = rules::RhaiScope::new();
    rules::bind_shell_anchors(&mut scope, shell);
    rules::bind_intent_derived(&mut scope, resolved, pip_size);
    match rules::resolve_tunable::<bool>(&engine, &mut scope, gate) {
        Ok(true) => AllowEntryOutcome::Proceed,
        Ok(false) => AllowEntryOutcome::Blocked,
        Err(err) => {
            let kind = match &err {
                RuleError::Parse(_) => "parse",
                RuleError::Eval(_) => "eval",
                RuleError::WrongType { .. } => "wrong-type",
            };
            AllowEntryOutcome::ScriptError {
                kind,
                message: err.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use trade_control_core::intent::{
        Action, BrokerKind, Direction, EntrySpec, PriceAnchor, PriceRef, ResolvedEntry, RiskBudget,
        TakeProfit,
    };
    use trade_control_core::tunable::Tunable;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn shell_with(signal_confirmed: Option<bool>) -> Shell {
        Shell {
            close: 1.1000,
            high: 1.1020,
            low: 1.0980,
            time: ts("2026-05-26T10:00:00Z"),
            signal_high: Some(1.1018),
            signal_low: Some(1.0982),
            signal_range: Some(0.0036),
            signal_start_time: Some(ts("2026-05-26T09:00:00Z")),
            signal_kind: None,
            golden: None,
            atr: None,
            signal_confirmed,
            recent_high: None,
            recent_low: None,
        }
    }

    fn resolved_long_market() -> Resolved {
        Resolved {
            id: "t1".into(),
            not_after: ts("2026-05-13T20:00:00Z"),
            instrument: "EUR_USD".into(),
            direction: Direction::Long,
            entry: ResolvedEntry::Market {
                reference_price: 1.1000,
            },
            stop_loss: 1.0978,
            take_profit: 1.1044,
            risk: RiskBudget::Percent(0.5),
            dry_run: false,
        }
    }

    fn intent_with_gate(gate: Option<Tunable<bool>>) -> Intent {
        Intent {
            v: 1,
            id: "msg-1".into(),
            not_before: None,
            not_after: ts("2026-06-01T00:00:00Z"),
            action: Action::Enter,
            instrument: "EUR_USD".into(),
            direction: Some(Direction::Long),
            entry: Some(EntrySpec::Market),
            stop_loss: Some(PriceRef::Anchored {
                from: PriceAnchor::Low,
                offset_pips: -2.0,
            }),
            take_profit: Some(TakeProfit::RMultiple {
                from: PriceAnchor::Close,
                offset_r: 2.0,
            }),
            risk_pct: Tunable::Static(0.5),
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
            trade_id: None,
            max_retries: trade_control_core::tunable::Tunable::Static(0),
            allow_entry: gate,
            needs_golden: false,
            blackout_id: None,
            news_id: None,
            require_news_window: None,
            reason: None,
        }
    }

    #[test]
    fn absent_gate_proceeds() {
        let outcome = evaluate(
            &intent_with_gate(None),
            &shell_with(None),
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome, AllowEntryOutcome::Proceed);
    }

    #[test]
    fn static_true_proceeds() {
        let outcome = evaluate(
            &intent_with_gate(Some(Tunable::Static(true))),
            &shell_with(None),
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome, AllowEntryOutcome::Proceed);
    }

    #[test]
    fn static_false_blocks() {
        let outcome = evaluate(
            &intent_with_gate(Some(Tunable::Static(false))),
            &shell_with(None),
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome, AllowEntryOutcome::Blocked);
    }

    #[test]
    fn script_reading_shell_anchor_evaluates() {
        // Canonical wait-for-confirmation gate.
        let gate = Tunable::from_script("signal_confirmed == true");
        let outcome_proceed = evaluate(
            &intent_with_gate(Some(gate.clone())),
            &shell_with(Some(true)),
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome_proceed, AllowEntryOutcome::Proceed);

        let outcome_block = evaluate(
            &intent_with_gate(Some(gate)),
            &shell_with(Some(false)),
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome_block, AllowEntryOutcome::Blocked);
    }

    #[test]
    fn script_reading_derived_geometry_evaluates() {
        // R = 2.0 on the fixture; require R >= 2 to enter.
        let gate = Tunable::from_script("r_multiple >= 2.0");
        let outcome = evaluate(
            &intent_with_gate(Some(gate)),
            &shell_with(None),
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome, AllowEntryOutcome::Proceed);
    }

    #[test]
    fn compound_script_with_pct_helper_evaluates() {
        // The candle-size override: confirm == true OR signal-range is
        // >= 10% of tp_distance. Here signal_range=0.0036, tp_distance
        // = 0.0044 → ~81%, well above 10%, so unconfirmed still passes.
        let gate = Tunable::from_script(
            "signal_confirmed == true || pct(signal_range, tp_distance) >= 10.0",
        );
        let outcome = evaluate(
            &intent_with_gate(Some(gate)),
            &shell_with(Some(false)),
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome, AllowEntryOutcome::Proceed);
    }

    #[test]
    fn script_parse_error_surfaces_kind() {
        let gate = Tunable::from_script("if if if");
        match evaluate(
            &intent_with_gate(Some(gate)),
            &shell_with(None),
            &resolved_long_market(),
            0.0001,
        ) {
            AllowEntryOutcome::ScriptError { kind, .. } => assert_eq!(kind, "parse"),
            other => panic!("expected ScriptError(parse), got {other:?}"),
        }
    }

    fn shell_with_golden(golden: Option<bool>) -> Shell {
        let mut shell = shell_with(None);
        shell.golden = golden;
        shell
    }

    fn intent_needs_golden(needs_golden: bool, gate: Option<Tunable<bool>>) -> Intent {
        let mut intent = intent_with_gate(gate);
        intent.needs_golden = needs_golden;
        intent
    }

    #[test]
    fn needs_golden_blocks_when_shell_missing_golden() {
        // `golden: None` is the realistic case — older Pine indicators
        // that don't carry the field at all. Conservative reject.
        let outcome = evaluate(
            &intent_needs_golden(true, None),
            &shell_with_golden(None),
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome, AllowEntryOutcome::NeedsGoldenUnmet);
    }

    #[test]
    fn needs_golden_blocks_when_shell_golden_false() {
        let outcome = evaluate(
            &intent_needs_golden(true, None),
            &shell_with_golden(Some(false)),
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome, AllowEntryOutcome::NeedsGoldenUnmet);
    }

    #[test]
    fn needs_golden_proceeds_when_shell_golden_true() {
        let outcome = evaluate(
            &intent_needs_golden(true, None),
            &shell_with_golden(Some(true)),
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome, AllowEntryOutcome::Proceed);
    }

    #[test]
    fn needs_golden_false_ignored() {
        // Default-off — should be a noop even with golden: None.
        let outcome = evaluate(
            &intent_needs_golden(false, None),
            &shell_with_golden(None),
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome, AllowEntryOutcome::Proceed);
    }

    #[test]
    fn needs_golden_runs_before_allow_entry_script() {
        // Golden check is cheap and ordered first; even if the script
        // would error, the gate short-circuits on needs_golden_unmet.
        let broken_script = Tunable::from_script("if if if");
        let outcome = evaluate(
            &intent_needs_golden(true, Some(broken_script)),
            &shell_with_golden(Some(false)),
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome, AllowEntryOutcome::NeedsGoldenUnmet);
    }

    #[test]
    fn needs_golden_and_allow_entry_script_compose_and() {
        // Both pass → proceed. Golden=true and script returns true.
        let gate = Tunable::from_script("signal_confirmed == true");
        let mut shell = shell_with_golden(Some(true));
        shell.signal_confirmed = Some(true);
        let outcome = evaluate(
            &intent_needs_golden(true, Some(gate.clone())),
            &shell,
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome, AllowEntryOutcome::Proceed);

        // Golden=true, script returns false → blocked by script.
        let mut shell_block = shell_with_golden(Some(true));
        shell_block.signal_confirmed = Some(false);
        let outcome_block = evaluate(
            &intent_needs_golden(true, Some(gate)),
            &shell_block,
            &resolved_long_market(),
            0.0001,
        );
        assert_eq!(outcome_block, AllowEntryOutcome::Blocked);
    }

    #[test]
    fn script_wrong_return_type_surfaces_kind() {
        let gate = Tunable::from_script("1.5");
        match evaluate(
            &intent_with_gate(Some(gate)),
            &shell_with(None),
            &resolved_long_market(),
            0.0001,
        ) {
            AllowEntryOutcome::ScriptError { kind, .. } => assert_eq!(kind, "wrong-type"),
            other => panic!("expected ScriptError(wrong-type), got {other:?}"),
        }
    }
}
