//! Parser for economic event values
//!
//! Handles parsing values like "0.5%", "220K", "1.5M", "-0.3%"

/// Parsed economic value as a normalized number
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParsedValue {
    /// The normalized numeric value
    pub value: f64,
    /// Whether the original had a percentage sign
    pub is_percentage: bool,
}

/// Parse an economic value string into a normalized number
///
/// Handles formats like:
/// - "0.5%" -> 0.5
/// - "-0.3%" -> -0.3
/// - "220K" -> 220000
/// - "1.5M" -> 1500000
/// - "2.3B" -> 2300000000
/// - "1,234" -> 1234
/// - "< 0.1%" -> 0.05 (estimate for "less than")
/// - "> 0.1%" -> 0.15 (estimate for "greater than")
///
/// Returns None if the value cannot be parsed
pub fn parse_economic_value(s: &str) -> Option<ParsedValue> {
    let s = s.trim();

    if s.is_empty() {
        return None;
    }

    // Handle "less than" and "greater than" markers
    let (s, modifier) = if let Some(rest) = s.strip_prefix('<') {
        (rest.trim(), -0.5)
    } else if let Some(rest) = s.strip_prefix('>') {
        (rest.trim(), 0.5)
    } else {
        (s, 0.0)
    };

    // Check for percentage
    let is_percentage = s.ends_with('%');
    let s = s.trim_end_matches('%').trim();

    // Check for K/M/B/T suffix
    let (s, multiplier) = if s.ends_with('K') || s.ends_with('k') {
        (s.trim_end_matches(['K', 'k']).trim(), 1_000.0)
    } else if s.ends_with('M') || s.ends_with('m') {
        (s.trim_end_matches(['M', 'm']).trim(), 1_000_000.0)
    } else if s.ends_with('B') || s.ends_with('b') {
        (s.trim_end_matches(['B', 'b']).trim(), 1_000_000_000.0)
    } else if s.ends_with('T') || s.ends_with('t') {
        (s.trim_end_matches(['T', 't']).trim(), 1_000_000_000_000.0)
    } else {
        (s, 1.0)
    };

    // Remove commas and whitespace
    let cleaned: String = s
        .chars()
        .filter(|c| *c != ',' && !c.is_whitespace())
        .collect();

    // Try to parse as a number
    let value: f64 = cleaned.parse().ok()?;

    // Apply multiplier and modifier
    let final_value =
        (value * multiplier) + (modifier * multiplier.max(1.0) * value.abs().max(0.1));

    Some(ParsedValue {
        value: final_value,
        is_percentage,
    })
}

/// Compare two values and determine if actual is better or worse than forecast
///
/// Returns:
/// - Some(true) if actual > forecast (generally positive)
/// - Some(false) if actual < forecast (generally negative)
/// - None if values are approximately equal
pub fn compare_values(actual: &str, forecast: &str) -> Option<bool> {
    let actual_val = parse_economic_value(actual)?;
    let forecast_val = parse_economic_value(forecast)?;

    // Use a small threshold for "approximately equal"
    let threshold = (forecast_val.value.abs() * 0.02).max(0.01);
    let diff = actual_val.value - forecast_val.value;

    if diff > threshold {
        Some(true)
    } else if diff < -threshold {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_percentage() {
        let result = parse_economic_value("0.5%").unwrap();
        assert!((result.value - 0.5).abs() < 0.001);
        assert!(result.is_percentage);
    }

    #[test]
    fn parse_negative_percentage() {
        let result = parse_economic_value("-0.3%").unwrap();
        assert!((result.value - (-0.3)).abs() < 0.001);
        assert!(result.is_percentage);
    }

    #[test]
    fn parse_k_suffix() {
        let result = parse_economic_value("220K").unwrap();
        assert!((result.value - 220_000.0).abs() < 0.001);
        assert!(!result.is_percentage);
    }

    #[test]
    fn parse_m_suffix() {
        let result = parse_economic_value("1.5M").unwrap();
        assert!((result.value - 1_500_000.0).abs() < 0.001);
    }

    #[test]
    fn parse_b_suffix() {
        let result = parse_economic_value("2.3B").unwrap();
        assert!((result.value - 2_300_000_000.0).abs() < 1.0);
    }

    #[test]
    fn parse_with_commas() {
        let result = parse_economic_value("1,234,567").unwrap();
        assert!((result.value - 1_234_567.0).abs() < 0.001);
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_economic_value("").is_none());
        assert!(parse_economic_value("   ").is_none());
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert!(parse_economic_value("abc").is_none());
        assert!(parse_economic_value("N/A").is_none());
    }

    #[test]
    fn compare_actual_beats_forecast() {
        assert_eq!(compare_values("0.8%", "0.5%"), Some(true));
        assert_eq!(compare_values("220K", "180K"), Some(true));
    }

    #[test]
    fn compare_actual_misses_forecast() {
        assert_eq!(compare_values("0.3%", "0.5%"), Some(false));
        assert_eq!(compare_values("150K", "180K"), Some(false));
    }

    #[test]
    fn compare_approximately_equal() {
        assert!(compare_values("0.50%", "0.51%").is_none());
    }

    #[test]
    fn parse_lowercase_suffix() {
        let result = parse_economic_value("220k").unwrap();
        assert!((result.value - 220_000.0).abs() < 0.001);
    }
}
