//! Pine `Candle Signals` indicator wiring. The plot IDs are tied to
//! the indicator's source-code plot order; whenever the operator
//! re-publishes the script with reordered plots, this file is the
//! single update point across the stack.
//!
//! Current shape (v2, 2026-05-26+):
//! - `plot_0..plot_9` — the 8 `signal_*` latches plus
//!   `recent_high` / `recent_low` for SL anchoring.
//! - `plot_10` — `Long Pattern` alertcondition.
//! - `plot_11` — `Short Pattern` alertcondition.

/// Pine study title as it appears in TradingView's data-source list.
pub const PINE_INDICATOR_NAME: &str = "Candle Signals";

/// Plot ID for the "Long Pattern" alertcondition. Used as the entry
/// trigger on long trades and as the close-on-reversal trigger on
/// short trades.
pub const PLOT_LONG_PATTERN: &str = "plot_10";

/// Plot ID for the "Short Pattern" alertcondition. Used as the entry
/// trigger on short trades and as the close-on-reversal trigger on
/// long trades.
pub const PLOT_SHORT_PATTERN: &str = "plot_11";

/// Trade direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Long trade (bullish reversal).
    Long,
    /// Short trade (bearish reversal).
    Short,
}

impl Direction {
    /// Lower-case string form (`"long"` / `"short"`) — matches the
    /// wire format used in trade specs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Long => "long",
            Self::Short => "short",
        }
    }

    /// Direction whose entry signal fires *opposite* to `self` — used
    /// to pick the close-on-reversal plot.
    pub fn opposite(self) -> Self {
        match self {
            Self::Long => Self::Short,
            Self::Short => Self::Long,
        }
    }
}

/// Pine plot ID for the entry signal in `direction`.
pub fn entry_plot_for(direction: Direction) -> &'static str {
    match direction {
        Direction::Long => PLOT_LONG_PATTERN,
        Direction::Short => PLOT_SHORT_PATTERN,
    }
}

/// Pine plot ID for the reversal-close signal of an open trade in
/// `direction`. (A long position closes on a bearish reversal, so it
/// listens to the Short Pattern plot; mirror for short.)
pub fn reversal_close_plot_for(direction: Direction) -> &'static str {
    entry_plot_for(direction.opposite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_plot_matches_direction() {
        assert_eq!(entry_plot_for(Direction::Long), "plot_10");
        assert_eq!(entry_plot_for(Direction::Short), "plot_11");
    }

    #[test]
    fn reversal_close_plot_inverts() {
        assert_eq!(reversal_close_plot_for(Direction::Long), "plot_11");
        assert_eq!(reversal_close_plot_for(Direction::Short), "plot_10");
    }

    #[test]
    fn direction_as_str_and_opposite() {
        assert_eq!(Direction::Long.as_str(), "long");
        assert_eq!(Direction::Short.as_str(), "short");
        assert_eq!(Direction::Long.opposite(), Direction::Short);
        assert_eq!(Direction::Short.opposite(), Direction::Long);
    }
}
