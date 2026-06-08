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

/// Plot ID for the "Every Bar Close" alertcondition (Pine v2.4+). This
/// fires every closed bar carrying only TradingView built-ins
/// (`close`/`high`/`low`/`time`, no plots) and is the per-bar heartbeat
/// the M/W enter intent binds to — M/W trades have no chart-side pattern
/// detection, so the worker recomputes the stop-entry/SL/TP each bar
/// close from baked path geometry plus the live shell.
///
/// The id follows the same declaration-order indexing as
/// [`PLOT_LONG_PATTERN`]/[`PLOT_SHORT_PATTERN`]: "Every Bar Close" is
/// declared immediately after the two pattern alertconditions, so it is
/// the next slot (`plot_12`). Verify against a live chart with a dry
/// `tv-arm` build after republishing the v2.4 study (the
/// `next_candle_timestamp` plots that landed in v2.3 sit *before* these,
/// so a stale-study mismatch surfaces as a "condition not found" on the
/// 05-enter alert — re-check this constant first if that happens).
pub const PLOT_EVERY_BAR_CLOSE: &str = "plot_12";

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

    /// Direction implied by an invalidation label. `too-high` invalidates a
    /// short trade (price went above the cap), `too-low` invalidates a long
    /// trade. Case-insensitive on ASCII; returns `None` for any other label.
    pub fn from_invalidation_label(lbl: &str) -> Option<Self> {
        let t = lbl.trim();
        if t.eq_ignore_ascii_case("too-high") {
            Some(Self::Short)
        } else if t.eq_ignore_ascii_case("too-low") {
            Some(Self::Long)
        } else {
            None
        }
    }
}

/// Direction implied by an M / W path-tool label. `m` (double-top) is a
/// short; `w` (double-bottom) is a long. The neutral alias `mw` carries
/// no direction on its own — the caller must derive it from geometry, so
/// this returns `None` for `mw` (and any non-M/W label). Case-insensitive
/// on ASCII.
pub fn mw_direction_from_label(lbl: &str) -> Option<Direction> {
    let t = lbl.trim();
    if t.eq_ignore_ascii_case("m") {
        Some(Direction::Short)
    } else if t.eq_ignore_ascii_case("w") {
        Some(Direction::Long)
    } else {
        None
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

    #[test]
    fn mw_direction_from_label_resolves() {
        assert_eq!(mw_direction_from_label("m"), Some(Direction::Short));
        assert_eq!(mw_direction_from_label("w"), Some(Direction::Long));
        assert_eq!(mw_direction_from_label("M"), Some(Direction::Short));
        assert_eq!(mw_direction_from_label("  w  "), Some(Direction::Long));
        // The neutral `mw` alias carries no direction on its own.
        assert_eq!(mw_direction_from_label("mw"), None);
        assert_eq!(mw_direction_from_label("neckline"), None);
        assert_eq!(mw_direction_from_label(""), None);
    }

    #[test]
    fn direction_from_invalidation_label() {
        assert_eq!(
            Direction::from_invalidation_label("too-high"),
            Some(Direction::Short)
        );
        assert_eq!(
            Direction::from_invalidation_label("too-low"),
            Some(Direction::Long)
        );
        assert_eq!(
            Direction::from_invalidation_label("TOO-HIGH"),
            Some(Direction::Short)
        );
        assert_eq!(
            Direction::from_invalidation_label("  too-low  "),
            Some(Direction::Long)
        );
        assert_eq!(Direction::from_invalidation_label("neckline"), None);
        assert_eq!(Direction::from_invalidation_label(""), None);
    }
}
