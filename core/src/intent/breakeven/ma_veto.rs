//! Shared MA policy selection for entry guards and both position-management paths.
use super::Breakeven;
use crate::{broker::Candle, intent::Direction, ma_cross, moving_average::MovingAverage};

pub const SMA_200_HISTORY_BARS: usize = 201;

impl Breakeven {
    pub fn has_ma_veto(self) -> bool {
        self.trigger.includes_ma_cross() || self.sma_200_break
    }

    pub fn ma_veto_history_bars(self) -> usize {
        if self.sma_200_break {
            SMA_200_HISTORY_BARS
        } else if self.trigger.includes_ma_cross() {
            ma_cross::HISTORY_BARS
        } else {
            0
        }
    }

    pub fn ma_veto_ready_at(self, candles: &[Candle], index: usize) -> bool {
        (!self.trigger.includes_ma_cross() || ma_cross::ready_at(candles, index))
            && (!self.sma_200_break
                || MovingAverage::standard_set()
                    .last()
                    .is_some_and(|average| ma_cross::average_ready_at(candles, index, *average)))
    }

    pub fn ma_veto_at(self, candles: &[Candle], index: usize, direction: Direction) -> bool {
        (self.trigger.includes_ma_cross() && ma_cross::adverse_cross_at(candles, index, direction))
            || (self.sma_200_break
                && MovingAverage::standard_set().last().is_some_and(|average| {
                    ma_cross::average_break_at(candles, index, *average, direction)
                }))
    }
}
