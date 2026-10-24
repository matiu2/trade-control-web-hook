//! Trend retracement preparation. The signal and order type remain independent.

use trade_control_core::intent::Direction;

use crate::{Candle, PlanState, TradePlan};

pub(crate) fn stamp_retrace(
    plan: &TradePlan,
    state: &mut PlanState,
    candle: &Candle,
    window: &[Candle],
) {
    let Some(line) = &plan.entry_conditions.retrace else {
        return;
    };
    if state.retrace_seen_at.is_some()
        || plan.armed_at.is_some_and(|armed| {
            candle.time + chrono::Duration::seconds(plan.granularity.seconds()) <= armed
        })
        || crate::spread_identity::suppress_plan(plan, candle.time)
    {
        return;
    }
    let Some(level) = crate::evaluate::line_price_at(
        &line.a,
        &line.b,
        candle,
        true,
        plan.granularity.seconds(),
        window,
    ) else {
        return;
    };
    // This is a settled retracement threshold, not an edge cross: already
    // beyond it (or a gap beyond it) also qualifies on a post-arm close.
    let reached = level.is_finite()
        && match plan.direction {
            Direction::Short => candle.c >= level,
            Direction::Long => candle.c <= level,
        };
    if reached {
        state.retrace_seen_at = Some(candle.time);
        tracing::info!(trade_id = %plan.trade_id, level, bar = %candle.time,
            "trend retracement satisfied by candle close; waiting for signal");
    }
}
