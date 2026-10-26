//! Reversible entry alignment through the shared pause/restoration mechanism.

use chrono::{DateTime, Utc};
use trade_control_core::{intent::Action, ma_cross, plan_eval::FiredIntent};

use crate::{Candle, PlanState, TradePlan};

pub(crate) fn satisfied(plan: &TradePlan, bar: DateTime<Utc>, window: &[Candle]) -> bool {
    !plan.entry_conditions.require_ma_alignment
        || window
            .iter()
            .position(|c| c.time == bar)
            .is_some_and(|i| ma_cross::favorable_at(window, i, plan.direction))
}

/// Refresh on every evaluated close: a failed control dispatch is retried on
/// the next bar instead of permanently losing a one-shot pause/resume edge.
/// Only this named pause is cleared; sibling pauses and vetoes are untouched.
pub(crate) fn control(
    plan: &TradePlan,
    state: &PlanState,
    candle: &Candle,
    window: &[Candle],
) -> Option<FiredIntent> {
    if !plan.entry_conditions.require_ma_alignment {
        return None;
    }
    let entry = plan
        .rules
        .iter()
        .find(|r| r.intent.action == Action::Enter)?;
    let favorable = !state.entries_blocked && satisfied(plan, candle.time, window);
    let action = if favorable {
        Action::Resume
    } else {
        Action::Pause
    };
    let verb = if favorable { "resume" } else { "pause" };
    let rule_id = format!("ma-alignment-{verb}");
    let mut intent = entry.intent.clone();
    intent.id = format!("{}-{rule_id}", plan.trade_id);
    intent.action = action;
    intent.trade_id = Some(plan.trade_id.clone());
    intent.blackout_id = Some(ma_cross::ALIGNMENT_PAUSE_ID.into());
    intent.reason = Some("requires favorable 9 EMA / 20 SMA alignment".into());
    // An entry leg can have a narrower starting window than its setup. The
    // hold must still protect any existing order before that leg can re-fire.
    intent.not_before = None;
    Some(FiredIntent {
        rule_id,
        intent,
        candle: *candle,
        signal: None,
    })
}
