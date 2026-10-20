//! Conservative offline account routing; never infer price or size conversions.

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, eyre};
use instrument_lookup::{AssetClass, Broker as CatalogBroker};
use trade_control_core::{
    account::AccountMetadata, intent::BrokerKind, plan_state::PlanState, trade_plan::TradePlan,
};

pub(super) fn reroute(
    plan: &TradePlan,
    target: &AccountMetadata,
    paper: bool,
) -> Result<TradePlan> {
    let mut next = plan.clone();
    let source = plan
        .rules
        .first()
        .map(|rule| rule.intent.broker)
        .ok_or_else(|| eyre!("cannot route a plan with no rules"))?;
    if plan.rules.iter().any(|rule| rule.intent.broker != source) {
        return Err(eyre!("plan rules disagree about their source broker"));
    }
    if plan
        .rules
        .iter()
        .any(|rule| rule.intent.risk_amount.is_some())
    {
        return Err(eyre!(
            "account currency for fixed risk_amount is not verified; re-arm this plan with explicit target-account sizing"
        ));
    }
    if source != target.broker {
        reroute_instrument(&mut next, target.broker)?;
    }
    next.shadow |= paper;
    next.rules.iter_mut().for_each(|rule| {
        rule.intent.account = Some(target.name.clone());
        rule.intent.broker = target.broker;
        rule.intent.instrument = next.instrument.clone();
        rule.intent.pip_size = Some(next.pip_size);
    });
    Ok(next)
}

fn reroute_instrument(plan: &mut TradePlan, target: BrokerKind) -> Result<()> {
    if plan.rules.iter().any(|rule| {
        rule.intent.size_units.is_some()
            || rule.intent.risk_amount.is_some()
            || rule.intent.contract_multiplier.is_some()
    }) {
        return Err(eyre!(
            "cross-broker switch requires percentage risk without fixed size, risk currency or contract multiplier; re-arm this plan explicitly"
        ));
    }
    let asset = instrument_lookup::resolve(&plan.instrument)?
        .ok_or_else(|| eyre!("no verified instrument mapping for {}", plan.instrument))?;
    if asset.class != AssetClass::Forex {
        return Err(eyre!(
            "cross-broker switching currently supports catalogued FX only; re-arm other asset classes explicitly"
        ));
    }
    let broker = match target {
        BrokerKind::TradeNation => CatalogBroker::TradeNation,
        BrokerKind::Oanda => CatalogBroker::Oanda,
        BrokerKind::Mt5 | BrokerKind::Ibkr => {
            return Err(eyre!(
                "cross-broker target needs an audited broker instrument mapping"
            ));
        }
    };
    let instrument = instrument_lookup::resolve_for(broker, &asset.id)?
        .ok_or_else(|| eyre!("target broker has no catalogued mapping for {}", asset.id))?;
    if !plan.pip_size.is_finite()
        || (plan.pip_size - instrument.pip_size).abs() > instrument.pip_size * 1e-9
    {
        return Err(eyre!(
            "source and target pip units differ; re-arm to convert price offsets explicitly"
        ));
    }
    if plan.rules.iter().any(|rule| {
        rule.intent.pip_size.is_some_and(|pip| {
            !pip.is_finite() || (pip - plan.pip_size).abs() > plan.pip_size * 1e-9
        })
    }) {
        return Err(eyre!("rule pip units disagree with the plan"));
    }
    plan.instrument = instrument.broker_symbol.clone();
    plan.pip_size = instrument.pip_size;
    plan.rules
        .iter_mut()
        .for_each(|rule| rule.intent.tick_size = Some(instrument.tick_size));
    Ok(())
}

pub(super) fn switch_state(mut state: PlanState, now: DateTime<Utc>) -> PlanState {
    // Bar timestamps are opening times: this also skips the bar forming now.
    state.watermark = Some(now);
    state.last_close.clear();
    state.origin_open.clear();
    state.last_confirmed_enter_at = Some(now);
    state
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use trade_control_core::{
        account::AccountKind,
        plan_state::{Phase, PlanState},
    };

    pub(crate) fn plan() -> TradePlan {
        serde_json::from_value(serde_json::json!({
            "trade_id":"audusd-test", "instrument":"AUDUSD", "direction":"long",
            "granularity":"h1", "pip_size":0.0001, "rules":[{
                "rule_id":"05-enter", "fire_mode":"every_bar", "kind":"enter",
                "trigger":{"type":"time_reached", "at_epoch":0},
                "intent":{"v":1,"id":"audusd-test-enter","not_after":"2026-11-01T00:00:00Z",
                    "action":"enter","instrument":"AUDUSD","account":"expired", "broker":"mt5",
                    "trade_id":"audusd-test", "pip_size":0.0001, "tick_size":0.00001}
            }]
        }))
        .expect("plan fixture")
    }

    #[test]
    fn mt5_fx_maps_to_target_symbol_and_preserves_identifiers() {
        let before = plan();
        let account = AccountMetadata::new("reversals", BrokerKind::TradeNation, AccountKind::Demo);
        let after = reroute(&before, &account, false).expect("known FX mapping");
        assert_eq!(after.instrument, "AUD/USD");
        assert_eq!(after.trade_id, before.trade_id);
        assert_eq!(after.rules[0].rule_id, before.rules[0].rule_id);
        assert_eq!(after.rules[0].intent.id, before.rules[0].intent.id);
        assert_eq!(after.rules[0].intent.account.as_deref(), Some("reversals"));
        assert_eq!(after.rules[0].intent.broker, BrokerKind::TradeNation);
        assert_eq!(after.rules[0].intent.instrument, after.instrument);
    }

    #[test]
    fn paper_cannot_be_reenabled_by_an_account_swap() {
        let mut before = plan();
        before.shadow = true;
        let account = AccountMetadata::new("reversals", BrokerKind::TradeNation, AccountKind::Demo);
        assert!(reroute(&before, &account, false).expect("map").shadow);
    }

    #[test]
    fn rejects_ambiguous_cross_broker_size_and_price_units() {
        let account = AccountMetadata::new("reversals", BrokerKind::TradeNation, AccountKind::Demo);
        let mut before = plan();
        before.rules[0].intent.size_units =
            Some(trade_control_core::tunable::Tunable::Static(1000.0));
        assert!(reroute(&before, &account, false).is_err());
        before.rules[0].intent.size_units = None;
        before.pip_size = 0.01;
        assert!(reroute(&before, &account, false).is_err());
        before.instrument = "US30".into();
        assert!(reroute(&before, &account, false).is_err());
    }

    #[test]
    fn switch_discards_pre_switch_signal_clock_but_preserves_prep_progress() {
        let now = "2026-10-21T12:34:56Z".parse().expect("now");
        let mut state = PlanState::seed(Phase::AwaitEntry, now);
        state.fired.insert("03-prep".into());
        state.last_close.insert("05-enter".into(), 0.7);
        state.origin_open.insert("05-enter".into(), 0.6);
        state.break_close_at = Some(now - chrono::Duration::hours(1));
        let after = switch_state(state.clone(), now);
        assert_eq!(after.watermark, Some(now));
        assert!(after.last_close.is_empty());
        assert!(after.origin_open.is_empty());
        assert_eq!(after.fired, state.fired);
        assert_eq!(after.break_close_at, state.break_close_at);
        assert_eq!(after.last_confirmed_enter_at, Some(now));
    }
}
