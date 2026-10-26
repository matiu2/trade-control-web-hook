//! Close-confirmed moving-average protection shared by live and replay.
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
    if !rule.ma_veto_at(candles, index, inputs.direction) {
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

#[cfg(test)]
mod sma_break_tests;

/// Both closes need their own contemporaneous average; one earlier bar seeds
/// the side of the break. Unknown values fail closed rather than guessing.
pub fn average_ready_at(candles: &[Candle], index: usize, average: MovingAverage) -> bool {
    index
        .checked_sub(1)
        .and_then(|i| average.value_at(candles, i))
        .is_some()
        && average.value_at(candles, index).is_some()
}

/// A fresh price break and CLOSE against the trade, not a wick touch. An
/// already-adverse close at arming is not itself a new break. Like MA cross,
/// departure from equality counts; equality on the new close does not.
pub fn average_break_at(
    candles: &[Candle],
    index: usize,
    average: MovingAverage,
    direction: Direction,
) -> bool {
    let relation = |i: usize| {
        candles
            .get(i)?
            .c
            .partial_cmp(&average.value_at(candles, i)?)
    };
    let adverse = match direction {
        Direction::Long => Ordering::Less,
        Direction::Short => Ordering::Greater,
    };
    relation(index) == Some(adverse)
        && index
            .checked_sub(1)
            .and_then(relation)
            .is_some_and(|previous| previous != adverse)
}
