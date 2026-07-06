//! Candle-quality gate.
//!
//! Single source of truth for the `needs_golden` / `needs_confirmed`
//! shell checks. Both gates are typed booleans on [`Intent`] (promoted
//! out of Rhai scripts so they read cleanly on the wire and short-circuit
//! before any Rhai engine spin-up); both reject when the relevant shell
//! field is anything other than `Some(true)`.
//!
//! Reused by [`crate::allow_entry_gate`] (Enter path) and the worker's
//! `allow_close_gate` (Close path on the consolidated reversal template).
//! AND-composed with the action's script gate — both this check and
//! `allow_entry` / `allow_close` must pass for the dispatch to reach the
//! broker.
//!
//! Lives in `core` so **both** the live worker and the offline replay
//! (`engine::simulator`) apply the same candle-quality gate; the worker
//! re-exports it so its call sites are unchanged.

use crate::intent::{Intent, Shell};

/// Outcome of the candle-quality gate. Maps onto a 412 rejection at the
/// dispatch layer; the variant tells the rejection-outcome string which
/// of the two checks failed.
#[derive(Debug, PartialEq, Eq)]
pub enum CandleGateOutcome {
    /// Both checks passed (or neither was set). Continue.
    Proceed,
    /// `needs_golden` was set but the shell did not carry
    /// `golden: Some(true)`. `None` (older Pine indicators that don't
    /// emit the field) is treated as `false` — conservative reject.
    NeedsGoldenUnmet,
    /// `needs_confirmed` was set but the shell did not carry
    /// `signal_confirmed: Some(true)`. Same `None`-rejects rule.
    NeedsConfirmedUnmet,
}

/// Run the gate. Order: `needs_golden` first (so the rejection-outcome
/// names the stricter-of-the-two check when both fail), then
/// `needs_confirmed`. Either check unset is a noop.
pub fn evaluate(intent: &Intent, shell: &Shell) -> CandleGateOutcome {
    if intent.needs_golden && shell.golden != Some(true) {
        return CandleGateOutcome::NeedsGoldenUnmet;
    }
    if intent.needs_confirmed && shell.signal_confirmed != Some(true) {
        return CandleGateOutcome::NeedsConfirmedUnmet;
    }
    CandleGateOutcome::Proceed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::{
        Action, BrokerKind, Direction, EntrySpec, PriceAnchor, PriceRef, TakeProfit,
    };
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

    fn enter_intent(needs_golden: bool, needs_confirmed: bool) -> Intent {
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
            max_retries: Tunable::Static(0),
            expiry_bars: None,
            allow_entry: None,
            allow_close: None,
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
            spread_window: None,
            trade_plan: None,
            blackout_close: crate::intent::BlackoutCloseAction::default(),
            breakeven: None,
            include_archived: false,
        }
    }

    #[test]
    fn neither_set_proceeds() {
        let outcome = evaluate(&enter_intent(false, false), &shell_with(None, None));
        assert_eq!(outcome, CandleGateOutcome::Proceed);
    }

    #[test]
    fn needs_golden_unmet_when_none() {
        let outcome = evaluate(&enter_intent(true, false), &shell_with(None, None));
        assert_eq!(outcome, CandleGateOutcome::NeedsGoldenUnmet);
    }

    #[test]
    fn needs_golden_unmet_when_false() {
        let outcome = evaluate(&enter_intent(true, false), &shell_with(Some(false), None));
        assert_eq!(outcome, CandleGateOutcome::NeedsGoldenUnmet);
    }

    #[test]
    fn needs_golden_proceeds_when_true() {
        let outcome = evaluate(&enter_intent(true, false), &shell_with(Some(true), None));
        assert_eq!(outcome, CandleGateOutcome::Proceed);
    }

    #[test]
    fn needs_confirmed_unmet_when_none() {
        let outcome = evaluate(&enter_intent(false, true), &shell_with(None, None));
        assert_eq!(outcome, CandleGateOutcome::NeedsConfirmedUnmet);
    }

    #[test]
    fn needs_confirmed_unmet_when_false() {
        let outcome = evaluate(&enter_intent(false, true), &shell_with(None, Some(false)));
        assert_eq!(outcome, CandleGateOutcome::NeedsConfirmedUnmet);
    }

    #[test]
    fn needs_confirmed_proceeds_when_true() {
        let outcome = evaluate(&enter_intent(false, true), &shell_with(None, Some(true)));
        assert_eq!(outcome, CandleGateOutcome::Proceed);
    }

    #[test]
    fn both_set_both_pass_proceeds() {
        let outcome = evaluate(
            &enter_intent(true, true),
            &shell_with(Some(true), Some(true)),
        );
        assert_eq!(outcome, CandleGateOutcome::Proceed);
    }

    #[test]
    fn both_set_golden_named_first_when_both_fail() {
        // Documenting precedence: golden is checked before confirmed so
        // the rejection-outcome string consistently names the stricter
        // check first when an operator wired both on by mistake.
        let outcome = evaluate(&enter_intent(true, true), &shell_with(None, None));
        assert_eq!(outcome, CandleGateOutcome::NeedsGoldenUnmet);
    }

    #[test]
    fn confirmed_named_when_only_confirmed_fails() {
        let outcome = evaluate(
            &enter_intent(true, true),
            &shell_with(Some(true), Some(false)),
        );
        assert_eq!(outcome, CandleGateOutcome::NeedsConfirmedUnmet);
    }
}
