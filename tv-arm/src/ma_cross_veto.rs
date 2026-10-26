//! Build signed MA protection once for command-line and chart GUI arms.
use trade_control_conventions::RuleKind;
use trade_control_core::intent::{Action, BreakevenTrigger, Intent, VetoLevel};
use trade_control_core::trade_plan::{ConditionRule, FireMode, TradePlan, Trigger};

use crate::ma_requirements::MaRequirements;

pub(crate) fn configure(
    plan: &mut TradePlan,
    requirements: MaRequirements<'_>,
) -> color_eyre::Result<()> {
    let entry = plan
        .rules
        .iter()
        .find(|r| r.intent.action == Action::Enter)
        .ok_or_else(|| color_eyre::eyre::eyre!("MA veto requires an entry rule"))?
        .intent
        .clone();
    let average = "200 sma"
        .parse()
        .map_err(|error: String| color_eyre::eyre::eyre!("{error}"))?;
    let guards = [
        (
            requirements.cross_veto,
            "ma-cross",
            Trigger::AdverseMaCross {
                dir: plan.direction,
            },
        ),
        (
            requirements.sma_200_break_veto,
            "sma-200-break",
            Trigger::AdverseMaBreak {
                average,
                dir: plan.direction,
            },
        ),
    ];
    for rule in &mut plan.rules {
        if rule.intent.action != Action::Enter {
            continue;
        }
        let percentage = rule.intent.breakeven.is_some();
        let be = rule.intent.breakeven.get_or_insert_default();
        if requirements.cross_veto {
            be.trigger = if percentage {
                BreakevenTrigger::PriceProgressOrMaCross
            } else {
                BreakevenTrigger::MaCrossOnly
            };
        } else if !percentage {
            be.trigger = BreakevenTrigger::MaProtectionOnly;
        }
        be.sma_200_break = requirements.sma_200_break_veto;
        rule.intent.vetos.extend(
            guards
                .iter()
                .filter(|(enabled, ..)| *enabled)
                .map(|(_, name, _)| (*name).into()),
        );
    }
    guards
        .into_iter()
        .filter(|(enabled, ..)| *enabled)
        .for_each(|(_, name, trigger)| append_guard(plan, &entry, name, trigger));
    Ok(())
}

fn append_guard(plan: &mut TradePlan, entry: &Intent, name: &str, trigger: Trigger) {
    let mut intent = trade_control_cli::build_veto_intent(
        &plan.instrument,
        &plan.trade_id,
        name,
        0,
        Some(VetoLevel::CancelPending),
        Vec::new(),
        plan.armed_at.unwrap_or(entry.not_after),
        name,
    );
    let rule_id = format!("01-veto-{name}");
    intent.id = format!("{}-{rule_id}", plan.trade_id);
    intent.account = entry.account.clone();
    intent.not_after = entry.not_after;
    plan.rules.push(ConditionRule {
        rule_id,
        trigger,
        fire_mode: FireMode::Once,
        intent,
        kind: RuleKind::SetupInvalidation,
    });
}
