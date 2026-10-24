//! One-time MA-touch preparation, independent of the entry signal's location.

use chrono::{DateTime, Utc};

use crate::{Candle, PlanState, TradePlan};

pub(crate) fn satisfied(plan: &TradePlan, state: &PlanState) -> bool {
    plan.entry_conditions.enable_on_ma_touch.is_empty() || state.ma_touch_seen_at.is_some()
}

/// A signal must print at or after every latched entry preparation.
pub(crate) fn signal_floor(state: &PlanState) -> Option<DateTime<Utc>> {
    [state.retrace_seen_at, state.ma_touch_seen_at]
        .into_iter()
        .flatten()
        .max()
}

pub(crate) fn stamp(plan: &TradePlan, state: &mut PlanState, candle: &Candle, window: &[Candle]) {
    let averages = &plan.entry_conditions.enable_on_ma_touch;
    if averages.is_empty() || state.ma_touch_seen_at.is_some() {
        return;
    }
    let start = [
        plan.armed_at,
        plan.replay_start
            .and_then(|at| DateTime::from_timestamp(at, 0)),
    ]
    .into_iter()
    .flatten()
    .max();
    // OHLC cannot prove whether a wick happened before or after arming inside
    // the same candle. Only fully post-arm candles may enable the setup.
    if start.is_some_and(|start| candle.time < start)
        || crate::spread_identity::suppress_plan(plan, candle.time)
    {
        return;
    }
    let Some(index) = window.iter().position(|bar| bar.time == candle.time) else {
        return;
    };
    let Some(average) = averages.iter().find(|ma| ma.touches_at(window, index)) else {
        return;
    };
    state.ma_touch_seen_at = Some(candle.time);
    tracing::info!(trade_id = %plan.trade_id, average = %average, bar = %candle.time,
        "MA touched; entry enabled for this and later qualifying signals");
}
