//! Named MA requirements keep the per-signal filter and one-time gate distinct.

use trade_control_core::moving_average::MovingAverage;

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct MaRequirements<'a> {
    pub signal_touch: &'a [MovingAverage],
    pub cross_veto: bool,
    pub enabling_touch: &'a [MovingAverage],
}

impl MaRequirements<'_> {
    pub fn is_empty(self) -> bool {
        self.signal_touch.is_empty() && self.enabling_touch.is_empty() && !self.cross_veto
    }
}
