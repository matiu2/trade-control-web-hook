//! Sign-time validation of `Tunable::Script` fields on an `Intent`.
//!
//! The worker happily runs a malformed script and turns the failure into
//! a 412 at fire time — but by then the signed alerts are already on
//! TradingView. Catching parse / wrong-type / unknown-variable errors at
//! `build-trade` time means the operator finds the typo before the keys
//! are minted.
//!
//! Strategy: build a fixture `Shell` + `Resolved` that exercises every
//! Phase 1 + Phase 2 binding with concrete (non-`()`) values, then
//! resolve each `Tunable::Script` field against the same scope the
//! worker would use. The fixture is intentionally "all-fields-present"
//! so unknown-variable errors surface only when the script genuinely
//! references something we don't bind — not when a particular runtime
//! shell happens to omit `signal_confirmed`.
//!
//! Today the only scripted field is `allow_entry: Option<Tunable<bool>>`.
//! When the per-field tunables in `C-tunable-fields` land, extend the
//! `check_*` set below — one helper per field, each delegating to
//! [`check_one`] with the matching `T`.

use trade_control_core::intent::{
    Direction, Intent, Resolved, ResolvedEntry, RiskBudget, Shell, SignalKind,
};
use trade_control_core::rules::{self, FromRhai, RuleError};
use trade_control_core::tunable::Tunable;

/// One validation failure. Field-name keyed so error messages can point
/// the operator at the offending YAML line.
#[derive(Debug, PartialEq)]
pub struct ScriptError {
    /// Intent field the script lives on (`allow_entry`, eventually
    /// `risk_pct`, etc.).
    pub field: &'static str,
    /// `parse` / `eval` / `wrong-type` — same vocabulary as
    /// `allow_entry_gate::AllowEntryOutcome::ScriptError`.
    pub kind: &'static str,
    /// Human-readable error surface.
    pub message: String,
}

impl core::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} script ({}): {}", self.field, self.kind, self.message)
    }
}

impl std::error::Error for ScriptError {}

/// Validate every `Tunable::Script` field on `intent`. Returns all
/// errors rather than short-circuiting on the first — operators editing
/// YAML by hand are likely to make a couple of mistakes at once and the
/// extra cost is negligible.
pub fn validate(intent: &Intent) -> Vec<ScriptError> {
    let shell = fixture_shell();
    let resolved = fixture_resolved();
    let pip_size = 0.0001;
    let mut errors = Vec::new();

    if let Some(t) = &intent.allow_entry
        && let Some(e) = check_one::<bool>("allow_entry", t, &shell, &resolved, pip_size)
    {
        errors.push(e);
    }
    if let Some(e) = check_one::<f64>("risk_pct", &intent.risk_pct, &shell, &resolved, pip_size) {
        errors.push(e);
    }
    if let Some(t) = &intent.risk_amount
        && let Some(e) = check_one::<f64>("risk_amount", t, &shell, &resolved, pip_size)
    {
        errors.push(e);
    }
    if let Some(t) = &intent.size_units
        && let Some(e) = check_one::<f64>("size_units", t, &shell, &resolved, pip_size)
    {
        errors.push(e);
    }
    if let Some(t) = &intent.min_r
        && let Some(e) = check_one::<f64>("min_r", t, &shell, &resolved, pip_size)
    {
        errors.push(e);
    }
    if let Some(e) = check_one::<u32>(
        "max_retries",
        &intent.max_retries,
        &shell,
        &resolved,
        pip_size,
    ) {
        errors.push(e);
    }
    if let Some(t) = &intent.cooldown_hours
        && let Some(e) = check_one::<u32>("cooldown_hours", t, &shell, &resolved, pip_size)
    {
        errors.push(e);
    }
    if let Some(e) = check_one::<u32>("ttl_hours", &intent.ttl_hours, &shell, &resolved, pip_size) {
        errors.push(e);
    }
    // Future per-field tunables go here as additional check_one calls.

    errors
}

fn check_one<T: FromRhai + Clone>(
    field: &'static str,
    tunable: &Tunable<T>,
    shell: &Shell,
    resolved: &Resolved,
    pip_size: f64,
) -> Option<ScriptError> {
    // Static values can't fail — short-circuit before touching the engine.
    if matches!(tunable, Tunable::Static(_)) {
        return None;
    }
    let engine = rules::build_engine();
    let mut scope = rules::RhaiScope::new();
    rules::bind_shell_anchors(&mut scope, shell);
    rules::bind_intent_derived(&mut scope, resolved, pip_size);
    match rules::resolve_tunable::<T>(&engine, &mut scope, tunable) {
        Ok(_) => None,
        Err(err) => {
            let kind = match &err {
                RuleError::Parse(_) => "parse",
                RuleError::Eval(_) => "eval",
                RuleError::WrongType { .. } => "wrong-type",
            };
            Some(ScriptError {
                field,
                kind,
                message: err.to_string(),
            })
        }
    }
}

/// Synthetic shell with every optional field populated. Concrete values
/// so a script like `signal_confirmed == true` resolves to a real bool
/// rather than a `() == bool` eval error — we're testing the script
/// shape, not whether a particular live shell will fire it.
fn fixture_shell() -> Shell {
    Shell {
        close: 1.1000,
        high: 1.1020,
        low: 1.0980,
        open: Some(1.0995),
        time: "2026-05-26T10:00:00Z".parse().unwrap_or_default(),
        signal_high: Some(1.1018),
        signal_low: Some(1.0982),
        signal_range: Some(0.0036),
        signal_start_time: Some("2026-05-26T09:00:00Z".parse().unwrap_or_default()),
        signal_kind: Some(SignalKind::Pinbar),
        golden: Some(true),
        atr: Some(0.0012),
        signal_confirmed: Some(true),
        recent_high: Some(1.1030),
        recent_low: Some(1.0975),
        next_candle_timestamp_1: None,
        next_candle_timestamp_2: None,
        next_candle_timestamp_3: None,
        next_candle_timestamp_4: None,
        next_candle_timestamp_5: None,
    }
}

/// Synthetic resolved geometry. Long market entry with R = 2.0 so
/// scripts that reference `r_multiple` get a representative number.
fn fixture_resolved() -> Resolved {
    Resolved {
        id: "validator-fixture".into(),
        not_after: "2026-06-01T00:00:00Z".parse().unwrap_or_default(),
        instrument: "EUR_USD".into(),
        direction: Direction::Long,
        entry: ResolvedEntry::Market {
            reference_price: 1.1000,
        },
        stop_loss: 1.0978,
        take_profit: 1.1044,
        risk: RiskBudget::Percent(0.5),
        dry_run: false,
        recover_entry: None,
        breakeven: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use trade_control_core::intent::{
        Action, BrokerKind, EntrySpec, PriceAnchor, PriceRef, TakeProfit,
    };

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn intent_with_allow_entry(gate: Option<Tunable<bool>>) -> Intent {
        Intent {
            entry_level_vetos: Vec::new(),
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
                offset_atr_pct: None,
            }),
            take_profit: Some(TakeProfit::Anchored(PriceRef::Absolute {
                absolute: 1.1044,
            })),
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
            max_retries: Tunable::Static(0),
            expiry_bars: None,
            allow_entry: gate,
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
            blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
            breakeven: None,
            include_archived: false,
        }
    }

    #[test]
    fn no_scripts_returns_empty() {
        let intent = intent_with_allow_entry(None);
        assert!(validate(&intent).is_empty());
    }

    #[test]
    fn static_tunable_is_not_evaluated() {
        // Static(false) is not the same as "script that returns false" —
        // the worker never touches the engine for Static, so the
        // validator shouldn't either.
        let intent = intent_with_allow_entry(Some(Tunable::Static(false)));
        assert!(validate(&intent).is_empty());
    }

    #[test]
    fn canonical_wait_for_confirmation_passes() {
        let intent =
            intent_with_allow_entry(Some(Tunable::from_script("signal_confirmed == true")));
        assert!(validate(&intent).is_empty());
    }

    #[test]
    fn compound_pct_script_passes() {
        // The candle-size override referenced in the plan.
        let intent = intent_with_allow_entry(Some(Tunable::from_script(
            "signal_confirmed == true || pct(signal_range, tp_distance) >= 10.0",
        )));
        assert!(validate(&intent).is_empty());
    }

    #[test]
    fn script_reading_derived_geometry_passes() {
        let intent = intent_with_allow_entry(Some(Tunable::from_script(
            "r_multiple >= 2.0 && direction == \"long\"",
        )));
        assert!(validate(&intent).is_empty());
    }

    #[test]
    fn parse_error_surfaces_with_field_name() {
        let intent = intent_with_allow_entry(Some(Tunable::from_script("if if if")));
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "allow_entry");
        assert_eq!(errs[0].kind, "parse");
    }

    #[test]
    fn wrong_return_type_surfaces() {
        // allow_entry expects bool; this script returns f64.
        let intent = intent_with_allow_entry(Some(Tunable::from_script("1.5")));
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "allow_entry");
        assert_eq!(errs[0].kind, "wrong-type");
    }

    #[test]
    fn risk_pct_script_passes_when_valid() {
        let mut intent = intent_with_allow_entry(None);
        intent.risk_pct = Tunable::from_script("if r_multiple >= 2.0 { 1.0 } else { 0.5 }");
        assert!(validate(&intent).is_empty());
    }

    #[test]
    fn risk_pct_script_parse_error_surfaces() {
        let mut intent = intent_with_allow_entry(None);
        intent.risk_pct = Tunable::from_script("if if if");
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "risk_pct");
        assert_eq!(errs[0].kind, "parse");
    }

    #[test]
    fn risk_pct_script_wrong_type_surfaces() {
        // Script returns bool, risk_pct expects f64.
        let mut intent = intent_with_allow_entry(None);
        intent.risk_pct = Tunable::from_script("true");
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "risk_pct");
        assert_eq!(errs[0].kind, "wrong-type");
    }

    #[test]
    fn both_fields_failing_produces_two_errors() {
        // The validator doesn't short-circuit — operators get the full
        // punch list rather than fix-one-find-the-next.
        let mut intent = intent_with_allow_entry(Some(Tunable::from_script("if if if")));
        intent.risk_pct = Tunable::from_script("nope");
        let errs = validate(&intent);
        assert_eq!(errs.len(), 2);
        let fields: Vec<&str> = errs.iter().map(|e| e.field).collect();
        assert!(fields.contains(&"allow_entry"));
        assert!(fields.contains(&"risk_pct"));
    }

    #[test]
    fn risk_amount_script_passes_when_valid() {
        let mut intent = intent_with_allow_entry(None);
        intent.risk_amount = Some(Tunable::from_script(
            "if r_multiple >= 2.0 { 2.0 } else { 1.0 }",
        ));
        assert!(validate(&intent).is_empty());
    }

    #[test]
    fn risk_amount_script_parse_error_surfaces() {
        let mut intent = intent_with_allow_entry(None);
        intent.risk_amount = Some(Tunable::from_script("if if if"));
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "risk_amount");
        assert_eq!(errs[0].kind, "parse");
    }

    #[test]
    fn risk_amount_script_wrong_type_surfaces() {
        // Script returns bool, risk_amount expects f64.
        let mut intent = intent_with_allow_entry(None);
        intent.risk_amount = Some(Tunable::from_script("true"));
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "risk_amount");
        assert_eq!(errs[0].kind, "wrong-type");
    }

    #[test]
    fn risk_pct_and_risk_amount_both_failing_produces_two_errors() {
        // Each field is validated independently — broken scripts on
        // both surface both errors. (Sizing-mode selection is the
        // resolver's job; risk_amount supersedes risk_pct's default
        // there, but that doesn't affect script-validity checks.)
        let mut intent = intent_with_allow_entry(None);
        intent.risk_pct = Tunable::from_script("nope");
        intent.risk_amount = Some(Tunable::from_script("also nope"));
        let errs = validate(&intent);
        assert_eq!(errs.len(), 2);
        let fields: Vec<&str> = errs.iter().map(|e| e.field).collect();
        assert!(fields.contains(&"risk_pct"));
        assert!(fields.contains(&"risk_amount"));
    }

    #[test]
    fn size_units_script_passes_when_valid() {
        let mut intent = intent_with_allow_entry(None);
        intent.size_units = Some(Tunable::from_script(
            "if r_multiple >= 2.0 { 0.02 } else { 0.01 }",
        ));
        assert!(validate(&intent).is_empty());
    }

    #[test]
    fn size_units_script_parse_error_surfaces() {
        let mut intent = intent_with_allow_entry(None);
        intent.size_units = Some(Tunable::from_script("if if if"));
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "size_units");
        assert_eq!(errs[0].kind, "parse");
    }

    #[test]
    fn size_units_script_wrong_type_surfaces() {
        let mut intent = intent_with_allow_entry(None);
        intent.size_units = Some(Tunable::from_script("true"));
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "size_units");
        assert_eq!(errs[0].kind, "wrong-type");
    }

    #[test]
    fn min_r_script_passes_when_valid() {
        let mut intent = intent_with_allow_entry(None);
        intent.min_r = Some(Tunable::from_script(
            "if r_multiple >= 2.0 { 1.5 } else { 1.0 }",
        ));
        assert!(validate(&intent).is_empty());
    }

    #[test]
    fn min_r_script_parse_error_surfaces() {
        let mut intent = intent_with_allow_entry(None);
        intent.min_r = Some(Tunable::from_script("if if if"));
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "min_r");
        assert_eq!(errs[0].kind, "parse");
    }

    #[test]
    fn min_r_script_wrong_type_surfaces() {
        let mut intent = intent_with_allow_entry(None);
        intent.min_r = Some(Tunable::from_script("true"));
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "min_r");
        assert_eq!(errs[0].kind, "wrong-type");
    }

    #[test]
    fn max_retries_script_passes_when_valid() {
        let mut intent = intent_with_allow_entry(None);
        intent.max_retries = Tunable::from_script("if golden == true { 5 } else { 3 }");
        intent.trade_id = Some("trade-mx-1".into());
        assert!(validate(&intent).is_empty());
    }

    #[test]
    fn max_retries_script_parse_error_surfaces() {
        let mut intent = intent_with_allow_entry(None);
        intent.max_retries = Tunable::from_script("if if if");
        intent.trade_id = Some("trade-mx-2".into());
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "max_retries");
        assert_eq!(errs[0].kind, "parse");
    }

    #[test]
    fn max_retries_script_wrong_type_surfaces() {
        // Script returns f64, max_retries expects u32.
        let mut intent = intent_with_allow_entry(None);
        intent.max_retries = Tunable::from_script("1.5");
        intent.trade_id = Some("trade-mx-3".into());
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "max_retries");
        assert_eq!(errs[0].kind, "wrong-type");
    }

    #[test]
    fn cooldown_hours_script_passes_when_valid() {
        let mut intent = intent_with_allow_entry(None);
        intent.cooldown_hours = Some(Tunable::from_script(
            "if signal_confirmed == true { 24 } else { 12 }",
        ));
        assert!(validate(&intent).is_empty());
    }

    #[test]
    fn cooldown_hours_script_parse_error_surfaces() {
        let mut intent = intent_with_allow_entry(None);
        intent.cooldown_hours = Some(Tunable::from_script("if if if"));
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "cooldown_hours");
        assert_eq!(errs[0].kind, "parse");
    }

    #[test]
    fn cooldown_hours_script_wrong_type_surfaces() {
        let mut intent = intent_with_allow_entry(None);
        intent.cooldown_hours = Some(Tunable::from_script("1.5"));
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "cooldown_hours");
        assert_eq!(errs[0].kind, "wrong-type");
    }

    #[test]
    fn ttl_hours_script_passes_when_valid() {
        let mut intent = intent_with_allow_entry(None);
        intent.ttl_hours = Tunable::from_script("if signal_confirmed == true { 8 } else { 4 }");
        assert!(validate(&intent).is_empty());
    }

    #[test]
    fn ttl_hours_script_parse_error_surfaces() {
        let mut intent = intent_with_allow_entry(None);
        intent.ttl_hours = Tunable::from_script("if if if");
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "ttl_hours");
        assert_eq!(errs[0].kind, "parse");
    }

    #[test]
    fn ttl_hours_script_wrong_type_surfaces() {
        let mut intent = intent_with_allow_entry(None);
        intent.ttl_hours = Tunable::from_script("1.5");
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "ttl_hours");
        assert_eq!(errs[0].kind, "wrong-type");
    }

    #[test]
    fn unknown_variable_surfaces_as_eval() {
        // Typo on a real binding name — Rhai reports VariableNotFound,
        // which we map to RuleError::Eval. This is the most common
        // failure mode for hand-edited YAML and the headline reason
        // the validator exists.
        let intent = intent_with_allow_entry(Some(Tunable::from_script("signal_confrmed == true")));
        let errs = validate(&intent);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "allow_entry");
        assert_eq!(errs[0].kind, "eval");
        assert!(
            errs[0].message.contains("signal_confrmed")
                || errs[0].message.to_lowercase().contains("variable"),
            "expected the typo or 'variable' in the message: {}",
            errs[0].message
        );
    }
}
