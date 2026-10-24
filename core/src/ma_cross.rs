//! Close-confirmed 9 EMA / 20 SMA reversal policy shared by live and replay.
use std::cmp::Ordering;

use crate::broker::Candle;
use crate::intent::Direction;
use crate::moving_average::MovingAverage;

/// Ninety bars seed the EMA; the preceding bar establishes the crossing side.
pub const HISTORY_BARS: usize = 91;

fn relation_at(candles: &[Candle], index: usize) -> Option<Ordering> {
    let mut averages = MovingAverage::standard_set().into_iter();
    let fast = averages.next()?.value_at(candles, index)?;
    let slow = averages.next()?.value_at(candles, index)?;
    fast.partial_cmp(&slow)
}

pub fn ready_at(candles: &[Candle], index: usize) -> bool {
    index
        .checked_sub(1)
        .and_then(|i| relation_at(candles, i))
        .is_some()
        && relation_at(candles, index).is_some()
}

/// A strict close beyond the other average, with the preceding close on or
/// across the opposite side (standard crossover/crossunder semantics). Equality
/// alone never fires. Only two MA values are read, so warmup depth cannot change
/// how a long sequence of equal averages is interpreted.
pub fn adverse_cross_at(candles: &[Candle], index: usize, direction: Direction) -> bool {
    let adverse = match direction {
        Direction::Long => Ordering::Less,
        Direction::Short => Ordering::Greater,
    };
    relation_at(candles, index) == Some(adverse)
        && index
            .checked_sub(1)
            .and_then(|i| relation_at(candles, i))
            .is_some_and(|previous| previous != adverse)
}

/// Executable exit price (bid for a long, ask for a short), never mid. A stop
/// exactly at market is invalid; an existing better stop must never be loosened.
pub fn tighten_to_fill(
    direction: Direction,
    fill: f64,
    current_stop: f64,
    exit_price: f64,
) -> Option<f64> {
    if ![fill, current_stop, exit_price]
        .into_iter()
        .all(f64::is_finite)
    {
        return None;
    }
    let permitted = match direction {
        Direction::Long => current_stop < fill && fill < exit_price,
        Direction::Short => current_stop > fill && fill > exit_price,
    };
    permitted.then_some(fill)
}

#[cfg(test)]
mod tests;

/// Shared position inputs keep fill, market and stop prices distinguishable.
#[derive(Clone, Copy)]
pub struct StopInputs {
    pub direction: Direction,
    pub fill: f64,
    pub current_stop: f64,
    pub exit_price: f64,
    /// Executable bid/ask close on the crossover candle. A losing event cannot
    /// be banked and applied on a later unrelated recovery.
    pub event_exit_price: f64,
}

pub fn cross_stop_at(
    rule: crate::intent::Breakeven,
    candles: &[Candle],
    index: usize,
    inputs: StopInputs,
) -> Option<f64> {
    if !rule.trigger.includes_ma_cross() || !adverse_cross_at(candles, index, inputs.direction) {
        return None;
    }
    tighten_to_fill(
        inputs.direction,
        inputs.fill,
        inputs.current_stop,
        inputs.event_exit_price,
    )?;
    tighten_to_fill(
        inputs.direction,
        inputs.fill,
        inputs.current_stop,
        inputs.exit_price,
    )
}
