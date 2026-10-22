//! Close-based entry averages. EMA uses an SMA seed over a fixed ten-period
//! tail, so live and replay agree even when their history windows differ.

use std::{fmt, num::NonZeroU16, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::broker::Candle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AverageKind {
    Ema,
    Sma,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MovingAverage {
    pub kind: AverageKind,
    pub period: NonZeroU16,
}

impl MovingAverage {
    pub fn standard_set() -> Vec<Self> {
        [
            (9, AverageKind::Ema),
            (20, AverageKind::Sma),
            (200, AverageKind::Sma),
        ]
        .into_iter()
        .filter_map(|(period, kind)| NonZeroU16::new(period).map(|period| Self { kind, period }))
        .collect()
    }

    /// Full warmup required at each candidate candle; unavailable averages
    /// cannot satisfy a touch requirement.
    pub fn history_bars(self) -> usize {
        let period = usize::from(self.period.get());
        match self.kind {
            AverageKind::Sma => period,
            AverageKind::Ema => period * 10,
        }
    }

    pub fn value_at(self, candles: &[Candle], index: usize) -> Option<f64> {
        let end = index.checked_add(1)?;
        let begin = end.checked_sub(self.history_bars())?;
        let tail = candles.get(begin..end)?;
        if tail.iter().any(|bar| !bar.c.is_finite()) {
            return None;
        }
        let period = usize::from(self.period.get());
        let seed = tail.get(..period)?.iter().map(|bar| bar.c).sum::<f64>() / period as f64;
        let alpha = 2.0 / (period as f64 + 1.0);
        let value = tail
            .iter()
            .skip(period)
            .fold(seed, |ema, bar| ema + alpha * (bar.c - ema));
        value.is_finite().then_some(value)
    }

    pub fn touches_pattern(
        self,
        candles: &[Candle],
        start: DateTime<Utc>,
        print: DateTime<Utc>,
    ) -> bool {
        // Require both endpoints: a truncated window cannot prove the pattern
        // touched. Never substitute the confirmation candle or aggregate range.
        if !candles.iter().any(|bar| bar.time == start)
            || !candles.iter().any(|bar| bar.time == print)
        {
            return false;
        }
        candles
            .iter()
            .enumerate()
            .filter(|(_, bar)| bar.time >= start && bar.time <= print)
            .any(|(index, bar)| {
                self.value_at(candles, index)
                    .is_some_and(|value| bar.l <= value && value <= bar.h)
            })
    }
}

impl FromStr for MovingAverage {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let text: String = input
            .chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect();
        let (digits, kind) = if let Some(digits) = text.strip_suffix("ema") {
            (digits, AverageKind::Ema)
        } else if let Some(digits) = text.strip_suffix("sma") {
            (digits, AverageKind::Sma)
        } else {
            return Err(format!(
                "invalid average {input:?}; use '9 ema' or '20 sma'"
            ));
        };
        let period = digits
            .parse::<NonZeroU16>()
            .map_err(|_| format!("invalid average period in {input:?}; use 1..=65535"))?;
        Ok(Self { kind, period })
    }
}

impl fmt::Display for MovingAverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            AverageKind::Ema => "ema",
            AverageKind::Sma => "sma",
        };
        write!(f, "{} {kind}", self.period)
    }
}
