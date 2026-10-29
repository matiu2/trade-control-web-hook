//! Directional signal location relative to a contemporary close-based average.

use chrono::{DateTime, Utc};

use super::MovingAverage;
use crate::{broker::Candle, intent::Direction};

impl MovingAverage {
    /// A short can touch or lie above the MA; a long can touch or lie below it.
    /// Only signal constituents count, with full MA history at each candle.
    pub fn offside_pattern(
        self,
        candles: &[Candle],
        start: DateTime<Utc>,
        print: DateTime<Utc>,
        direction: Direction,
    ) -> bool {
        self.pattern_matches(candles, start, print, |index| {
            self.value_at(candles, index).is_some_and(|value| {
                let bar = &candles[index];
                match direction {
                    Direction::Short => bar.h >= value,
                    Direction::Long => bar.l <= value,
                }
            })
        })
    }
}
