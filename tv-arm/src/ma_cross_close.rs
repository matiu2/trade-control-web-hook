//! Independent early exit; no entry veto, pause, or break-even policy.

use color_eyre::eyre::{Result, eyre};
use trade_control_conventions::RuleKind;
use trade_control_core::intent::Action;
use trade_control_core::trade_plan::{ConditionRule, FireMode, TradePlan, Trigger};

pub(crate) fn configure(plan: &mut TradePlan) -> Result<()> {
    let entry = &plan
        .rules
        .iter()
        .find(|r| r.intent.action == Action::Enter)
        .ok_or_else(|| eyre!("MA close requires an entry rule"))?
        .intent;
    let rule_id = "08-close-on-ma-cross";
    let mut intent = trade_control_cli::build_close_intent(
        &plan.instrument,
        &plan.trade_id,
        plan.armed_at.unwrap_or(entry.not_after),
        "ma-cross",
    );
    intent.id = format!("{}-{rule_id}", plan.trade_id);
    intent.account = entry.account.clone();
    intent.broker = entry.broker;
    intent.not_after = entry.not_after;
    intent.reason = Some("adverse-ma-cross".into());
    plan.rules.push(ConditionRule {
        rule_id: rule_id.into(),
        trigger: Trigger::AdverseMaCross {
            dir: plan.direction,
        },
        fire_mode: FireMode::EveryBar,
        intent,
        kind: RuleKind::PerPositionClose,
    });
    Ok(())
}
