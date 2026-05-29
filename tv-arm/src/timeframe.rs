//! Map a TradingView chart resolution string to a `calendar-bars`
//! timeframe argument.
//!
//! TradingView resolutions are minute counts as strings (`"1"`,
//! `"15"`, `"60"`, `"240"`) or letter-suffixed for daily and above
//! (`"D"`, `"W"`, `"M"`). `trade-calendar-maker` only knows two
//! window shapes — M15 (3h pause, Medium+ impact) and H1Plus
//! (8h pause, High impact only) — so this module collapses every
//! TV resolution down to one of those, or `None` for sub-15m charts
//! where the operator isn't holding through news anyway.

use trade_control_cli::TimeframeArg;

/// Returns the `--timeframe` that should be passed to
/// `trade-control calendar-bars` for a chart on `resolution`.
///
/// Returns `None` when the resolution is below 15 minutes — calendar
/// bars don't help on scalp charts.
pub fn infer_calendar_timeframe(resolution: &str) -> Option<TimeframeArg> {
    let s = resolution.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<u32>() {
        if n < 15 {
            return None;
        }
        if n == 15 {
            return Some(TimeframeArg::M15);
        }
        return Some(TimeframeArg::H1plus);
    }
    // Letter-suffixed (D, W, M, …) — treat as h1plus.
    Some(TimeframeArg::H1plus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifteen_minutes_is_m15() {
        assert_eq!(infer_calendar_timeframe("15"), Some(TimeframeArg::M15));
    }

    #[test]
    fn one_hour_and_up_is_h1plus() {
        for res in ["60", "240", "480"] {
            assert_eq!(
                infer_calendar_timeframe(res),
                Some(TimeframeArg::H1plus),
                "res {res:?}"
            );
        }
    }

    #[test]
    fn sub_fifteen_returns_none() {
        assert_eq!(infer_calendar_timeframe("1"), None);
        assert_eq!(infer_calendar_timeframe("5"), None);
        assert_eq!(infer_calendar_timeframe("14"), None);
    }

    #[test]
    fn empty_returns_none() {
        assert_eq!(infer_calendar_timeframe(""), None);
        assert_eq!(infer_calendar_timeframe("   "), None);
    }

    #[test]
    fn daily_and_above_is_h1plus() {
        for res in ["D", "W", "M", "3D", "1W"] {
            assert_eq!(
                infer_calendar_timeframe(res),
                Some(TimeframeArg::H1plus),
                "res {res:?}"
            );
        }
    }
}
