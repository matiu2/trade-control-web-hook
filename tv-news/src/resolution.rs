//! Parse TradingView's resolution string (`"15"`, `"60"`, `"D"`, ...)
//! into a bar width in seconds. Used by the bucketer to group news
//! events that land in the same chart bar.
//!
//! Resolutions supported (matches what TradingView Desktop exposes via
//! tv-mcp `state.resolution`):
//!
//! - Bare integer → minutes. `"1"`, `"5"`, `"15"`, `"60"`, `"240"`.
//! - `<n>S` → seconds. `"15S"` = 15 seconds. Rare but legal.
//! - `D` or `<n>D` → days. `"D"` = 1 day, `"3D"` = 3 days.
//! - `W` or `<n>W` → weeks.
//! - `M` or `<n>M` → months, approximated as 30 days (precision isn't
//!   load-bearing — this only decides which events share a bar).
//!
//! Unknown shapes return `None`; the caller falls back to a default
//! bar width (currently the H1 = 3600s the operator most often runs).

/// Parse a TradingView resolution string into bar width in seconds.
///
/// Returns `None` if the string isn't recognised — the caller decides
/// the fallback.
pub fn resolution_to_secs(res: &str) -> Option<i64> {
    let s = res.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(mins) = s.parse::<i64>() {
        // Bare integer: minutes. Reject 0 and negatives.
        return if mins > 0 { Some(mins * 60) } else { None };
    }
    // Suffix forms: "S", "D", "W", "M" with optional leading integer.
    let (num, suffix) = s.split_at(s.len() - 1);
    let n: i64 = if num.is_empty() { 1 } else { num.parse().ok()? };
    if n <= 0 {
        return None;
    }
    match suffix {
        "S" | "s" => Some(n),
        "D" | "d" => Some(n * 86_400),
        "W" | "w" => Some(n * 7 * 86_400),
        // Months — TradingView itself uses 30 days as the practical
        // bar width for monthly bars in its grid math.
        "M" | "m" => Some(n * 30 * 86_400),
        _ => None,
    }
}

/// Default bar width when the chart's resolution string can't be
/// parsed. H1 = 3600s — the granularity tv-news is most commonly run
/// against. Keeps the bucketing functional rather than degenerate
/// (each event in its own bucket).
pub const DEFAULT_BAR_SECS: i64 = 3600;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minute_integers() {
        assert_eq!(resolution_to_secs("1"), Some(60));
        assert_eq!(resolution_to_secs("5"), Some(300));
        assert_eq!(resolution_to_secs("15"), Some(900));
        assert_eq!(resolution_to_secs("60"), Some(3600));
        assert_eq!(resolution_to_secs("240"), Some(14_400));
    }

    #[test]
    fn parses_day_week_month_suffix() {
        assert_eq!(resolution_to_secs("D"), Some(86_400));
        assert_eq!(resolution_to_secs("3D"), Some(3 * 86_400));
        assert_eq!(resolution_to_secs("W"), Some(7 * 86_400));
        assert_eq!(resolution_to_secs("M"), Some(30 * 86_400));
        assert_eq!(resolution_to_secs("2M"), Some(60 * 86_400));
    }

    #[test]
    fn parses_seconds_suffix() {
        assert_eq!(resolution_to_secs("15S"), Some(15));
        assert_eq!(resolution_to_secs("1S"), Some(1));
    }

    #[test]
    fn case_insensitive_suffix() {
        assert_eq!(resolution_to_secs("d"), Some(86_400));
        assert_eq!(resolution_to_secs("w"), Some(7 * 86_400));
    }

    #[test]
    fn handles_whitespace() {
        assert_eq!(resolution_to_secs(" 60 "), Some(3600));
        assert_eq!(resolution_to_secs(" D "), Some(86_400));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(resolution_to_secs(""), None);
        assert_eq!(resolution_to_secs("0"), None);
        assert_eq!(resolution_to_secs("-5"), None);
        assert_eq!(resolution_to_secs("abc"), None);
        assert_eq!(resolution_to_secs("60X"), None);
    }
}
