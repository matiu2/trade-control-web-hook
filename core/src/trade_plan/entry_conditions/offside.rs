//! Combined signal-location requirements shared by live and replay.

use chrono::{DateTime, Utc};

use super::EntryConditions;
use crate::{broker::Candle, intent::Direction};

impl EntryConditions {
    pub fn pattern_qualifies(
        &self,
        candles: &[Candle],
        start: DateTime<Utc>,
        print: DateTime<Utc>,
        direction: Direction,
    ) -> bool {
        self.pattern_touches(candles, start, print)
            && (self.be_offside.is_empty()
                || self
                    .be_offside
                    .iter()
                    .any(|ma| ma.offside_pattern(candles, start, print, direction)))
    }
}
