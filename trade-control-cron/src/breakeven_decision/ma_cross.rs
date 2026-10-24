//! MA crossover adapter for the position watcher.
use super::*;

/// Cross arming retains the pre-fill history needed to seed the averages, but
/// only a close after the fill may act. The current executable quote must still
/// permit a tightening stop. Missing fill metadata fails closed (never trigger
/// geometry as a substitute). Percentage-based management remains independent.
pub fn decide_ma_cross(
    inputs: &BreakevenInputs<'_>,
    candles: &[trade_control_core::broker::BidAskCandle],
    now: DateTime<Utc>,
    account: Option<&str>,
    exit_price: f64,
) -> BreakevenDecision {
    use trade_control_core::ma_cross::{StopInputs, cross_stop_at};
    if !inputs.snapshot.rule.trigger.includes_ma_cross() {
        return BreakevenDecision::Hold;
    }
    let Some(fill_at) = inputs.position.opened_at else {
        return BreakevenDecision::Blocked(BreakevenBlock::NoFillTime);
    };
    let Some(fill) = inputs.position.entry_price else {
        tracing::error!(position_id = %inputs.position.position_id, "MA-cross break-even needs actual fill price; leaving stop unchanged");
        return BreakevenDecision::Hold;
    };
    let key = trade_control_core::spread_blackout::spread_lookup_key(
        &inputs.position.instrument,
        account,
    );
    let bar = Duration::seconds(inputs.snapshot.granularity.seconds());
    let mid: Vec<_> = candles
        .iter()
        .map(trade_control_core::broker::BidAskCandle::mid)
        .collect();
    if !mid
        .len()
        .checked_sub(1)
        .is_some_and(|i| trade_control_core::ma_cross::ready_at(&mid, i))
    {
        tracing::warn!("MA-cross stop has insufficient history; leaving stop unchanged");
        return BreakevenDecision::Hold;
    }
    candles
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, c)| c.time + bar > fill_at && c.time + bar <= now)
        .filter(|(_, c)| !is_inside_a_widen(&key, c.time))
        .find_map(|(index, c)| {
            let event_exit_price = match inputs.position.direction {
                trade_control_core::intent::Direction::Long => c.bid_c,
                trade_control_core::intent::Direction::Short => c.ask_c,
            };
            let stop_inputs = StopInputs {
                direction: inputs.position.direction,
                fill,
                current_stop: inputs.current_stop,
                exit_price,
                event_exit_price,
            };
            cross_stop_at(inputs.snapshot.rule, &mid, index, stop_inputs).map(|new_stop| {
                BreakevenDecision::Amend {
                    new_stop,
                    armed_by: c.c,
                    at: c.time,
                }
            })
        })
        .unwrap_or(BreakevenDecision::Hold)
}
