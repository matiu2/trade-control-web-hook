//! Build the same signed MA policy for command-line and chart GUI arms.
use trade_control_conventions::RuleKind;
use trade_control_core::intent::{Action, BreakevenTrigger, VetoLevel};
use trade_control_core::trade_plan::{ConditionRule, FireMode, TradePlan, Trigger};

pub(crate) fn configure(plan: &mut TradePlan) -> color_eyre::Result<()> {
    let entry = plan
        .rules
        .iter()
        .find(|r| r.intent.action == Action::Enter)
        .ok_or_else(|| color_eyre::eyre::eyre!("MA-cross veto requires an entry rule"))?
        .intent
        .clone();
    for rule in &mut plan.rules {
        if rule.intent.action != Action::Enter {
            continue;
        }
        let trigger = if rule.intent.breakeven.is_some() {
            BreakevenTrigger::PriceProgressOrMaCross
        } else {
            BreakevenTrigger::MaCrossOnly
        };
        rule.intent.breakeven.get_or_insert_default().trigger = trigger;
        rule.intent.vetos.push("ma-cross".into());
    }
    let mut intent = trade_control_cli::build_veto_intent(
        &plan.instrument,
        &plan.trade_id,
        "ma-cross",
        0,
        Some(VetoLevel::CancelPending),
        Vec::new(),
        plan.armed_at.unwrap_or(entry.not_after),
        "ma-cross",
    );
    intent.id = format!("{}-01-veto-ma-cross", plan.trade_id);
    intent.account = entry.account.clone();
    intent.not_after = entry.not_after;
    plan.rules.push(ConditionRule {
        rule_id: "01-veto-ma-cross".into(),
        trigger: Trigger::AdverseMaCross {
            dir: plan.direction,
        },
        fire_mode: FireMode::Once,
        intent,
        kind: RuleKind::SetupInvalidation,
    });
    Ok(())
}
