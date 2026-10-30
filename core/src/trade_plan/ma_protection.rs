//! MA exits need history and continued monitoring without gating entries.

use super::{ConditionRule, TradePlan, Trigger};
use crate::{broker::Candle, intent::Action, ma_cross};

impl TradePlan {
    /// History for all MA guards, including independent position closes.
    pub fn ma_guard_history_bars(&self) -> usize {
        self.rules.iter().map(history_bars).max().unwrap_or(0)
    }

    pub fn has_ma_guard(&self) -> bool {
        self.ma_guard_history_bars() > 0
    }

    /// Only entry-veto guards may withhold entries on insufficient history.
    pub fn ma_veto_history_bars(&self) -> usize {
        self.rules
            .iter()
            .filter(|r| r.intent.action == Action::Veto)
            .map(history_bars)
            .max()
            .unwrap_or(0)
    }

    pub fn has_ma_veto(&self) -> bool {
        self.ma_veto_history_bars() > 0
    }

    pub fn ma_veto_ready_at(&self, candles: &[Candle], index: usize) -> bool {
        self.rules
            .iter()
            .filter(|r| r.intent.action == Action::Veto)
            .all(|r| match r.trigger {
                Trigger::AdverseMaCross { .. } => ma_cross::ready_at(candles, index),
                Trigger::AdverseMaBreak { average, .. } => {
                    ma_cross::average_ready_at(candles, index, average)
                }
                _ => true,
            })
    }
}

fn history_bars(rule: &ConditionRule) -> usize {
    match rule.trigger {
        Trigger::AdverseMaCross { .. } => ma_cross::HISTORY_BARS,
        Trigger::AdverseMaBreak { average, .. } => average.history_bars() + 1,
        _ => 0,
    }
}
