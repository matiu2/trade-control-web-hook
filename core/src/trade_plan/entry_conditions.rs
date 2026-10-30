//! Optional entry preparation, shared by live evaluation and replay.

use serde::{Deserialize, Serialize};

use super::LinePoint;
use crate::moving_average::MovingAverage;

mod offside;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntryConditions {
    /// Pause entries and hold unfilled orders while 9 EMA / 20 SMA alignment
    /// is unfavorable. Clears independently of permanent vetoes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_ma_alignment: bool,
    /// A post-arm candle touching any listed MA enables subsequent signals.
    /// Unlike `must_touch`, this is a one-time milestone, persisted in state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enable_on_ma_touch: Vec<MovingAverage>,
    /// The signal pattern must touch at least one listed close-based average.
    /// Evaluated at each constituent candle, never at its later confirmation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must_touch: Vec<MovingAverage>,
    /// The signal must touch or lie above any selected MA for shorts, below
    /// for longs. Uses each constituent's own MA, excluding confirmation bars.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub be_offside: Vec<MovingAverage>,
    /// Under `--trend`, the optional neckline is a minimum retracement line.
    /// Short: close above it; long: close below it. No retest follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrace: Option<RetraceLine>,
}

impl EntryConditions {
    pub fn is_empty(&self) -> bool {
        !self.require_ma_alignment
            && self.retrace.is_none()
            && self.must_touch.is_empty()
            && self.be_offside.is_empty()
            && self.enable_on_ma_touch.is_empty()
    }

    pub fn history_bars(&self) -> usize {
        self.must_touch
            .iter()
            .chain(self.enable_on_ma_touch.iter())
            .chain(self.be_offside.iter())
            .map(|ma| ma.history_bars())
            .max()
            .unwrap_or(0)
            .max(if self.require_ma_alignment {
                crate::ma_cross::HISTORY_BARS
            } else {
                0
            })
    }

    pub fn pattern_touches(
        &self,
        candles: &[crate::broker::Candle],
        start: chrono::DateTime<chrono::Utc>,
        print: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        self.must_touch.is_empty()
            || self
                .must_touch
                .iter()
                .any(|ma| ma.touches_pattern(candles, start, print))
    }
}

impl super::TradePlan {
    pub fn detector_history_bars(&self) -> usize {
        let cfg = crate::signals::DetectorConfig::pine_defaults(self.granularity);
        crate::signals::detector_lookback_bars(&cfg, self.granularity)
            .max(self.entry_conditions.history_bars() + cfg.confirm_bars + 3)
            .max(self.ma_guard_history_bars())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetraceLine {
    pub a: LinePoint,
    pub b: LinePoint,
}

#[cfg(test)]
mod offside_tests;
#[cfg(test)]
mod tests;
