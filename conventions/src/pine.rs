//! Pine `Candle Signals` indicator wiring.
//!
//! Alertconditions are bound by their **title** (`"Long Pattern"`,
//! `"Short Pattern"`, `"Every Bar Close"`), not by a positional
//! `plot_N` id. The tv-arm JS template resolves title → live `plot_N`
//! at create-alert time from the study's `metaInfo()` — it filters
//! `metaInfo().plots` to the `alertcondition`-typed entries and maps
//! `metaInfo().styles[id].title` back to the id.
//!
//! This makes the binding immune to plot reordering: any `plot()`
//! added or removed ahead of the alertconditions shifts the `plot_N`
//! indices, but the title is stable. (Before 2026-06 these were
//! hardcoded `plot_10`/`plot_11`/`plot_12`; v2.3's five
//! `next_candle_timestamp_1..5` plots silently shifted them to
//! 15/16/17, which surfaced as `err.code="general"` on 05-enter. The
//! title-based resolver removes that whole failure class.)
//!
//! The only thing this file still pins by string is the alertcondition
//! *titles* — change those only in lockstep with the `alertcondition()`
//! calls in `pine-scripts/candle-signals-v2.pine`.

/// Pine study title as it appears in TradingView's data-source list.
pub const PINE_INDICATOR_NAME: &str = "Candle Signals";

/// Title of the "Long Pattern" alertcondition. Used as the entry
/// trigger on long trades and as the close-on-reversal trigger on
/// short trades. Matches the second arg of the `alertcondition()` call
/// in the Pine source.
pub const ALERT_LONG_PATTERN: &str = "Long Pattern";

/// Title of the "Short Pattern" alertcondition. Used as the entry
/// trigger on short trades and as the close-on-reversal trigger on
/// long trades.
pub const ALERT_SHORT_PATTERN: &str = "Short Pattern";

/// Title of the "Every Bar Close" alertcondition (Pine v2.4+). This
/// fires every closed bar carrying only TradingView built-ins
/// (`close`/`high`/`low`/`time`, no plots) and is the per-bar heartbeat
/// the M/W enter intent binds to — M/W trades have no chart-side pattern
/// detection, so the worker recomputes the stop-entry/SL/TP each bar
/// close from baked path geometry plus the live shell.
pub const ALERT_EVERY_BAR_CLOSE: &str = "Every Bar Close";

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

/// Alertcondition title for the entry signal in `direction`. The JS
/// template resolves this to the live `plot_N` at create-alert time.
pub fn entry_alert_for(direction: Direction) -> &'static str {
    match direction {
        Direction::Long => ALERT_LONG_PATTERN,
        Direction::Short => ALERT_SHORT_PATTERN,
    }
}

/// Alertcondition title for the reversal-close signal of an open trade
/// in `direction`. (A long position closes on a bearish reversal, so it
/// listens to the Short Pattern condition; mirror for short.)
pub fn reversal_close_alert_for(direction: Direction) -> &'static str {
    entry_alert_for(direction.opposite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_alert_matches_direction() {
        assert_eq!(entry_alert_for(Direction::Long), "Long Pattern");
        assert_eq!(entry_alert_for(Direction::Short), "Short Pattern");
    }

    #[test]
    fn reversal_close_alert_inverts() {
        assert_eq!(reversal_close_alert_for(Direction::Long), "Short Pattern");
        assert_eq!(reversal_close_alert_for(Direction::Short), "Long Pattern");
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
