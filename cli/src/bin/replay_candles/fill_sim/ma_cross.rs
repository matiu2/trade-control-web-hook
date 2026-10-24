//! The replay adapter retains warmup but delegates every decision to core.
use super::*;
use trade_control_core::ma_cross::{StopInputs, cross_stop_at};

pub(super) struct CrossHistory(
    Option<Vec<trade_control_core::broker::Candle>>,
    chrono::DateTime<chrono::Utc>,
);
impl CrossHistory {
    pub(super) fn new(
        resolved: &Resolved,
        candles: &[BidAskCandle],
        fire_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self(
            resolved
                .breakeven
                .filter(|be| be.trigger.includes_ma_cross())
                .map(|_| candles.iter().map(BidAskCandle::mid).collect()),
            fire_at,
        )
    }
    pub(super) fn stop(
        &self,
        resolved: &Resolved,
        candle: &BidAskCandle,
        fill: f64,
        stop: f64,
    ) -> Option<f64> {
        if candle.time <= self.1 {
            return None;
        }
        let candles = self.0.as_ref()?;
        let index = candles
            .binary_search_by_key(&candle.time, |c| c.time)
            .ok()?;
        let exit_price = match resolved.direction {
            Direction::Long => candle.bid_c,
            Direction::Short => candle.ask_c,
        };
        cross_stop_at(
            resolved.breakeven?,
            candles,
            index,
            StopInputs {
                direction: resolved.direction,
                fill,
                current_stop: stop,
                exit_price,
                event_exit_price: exit_price,
            },
        )
    }
}
