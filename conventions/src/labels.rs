//! Drawing-label vocabularies the chart reader accepts. The operator
//! writes one of these (case-insensitive) as the `text` property on a
//! TradingView drawing to assign it a role.
//!
//! Each vocabulary is an ordered list of synonyms — the first entry
//! is the canonical name, the rest are accepted aliases.

/// Trend-line label for the neckline / break-and-close prep.
pub const BREAK_LABELS: &[&str] = &["neckline", "break-and-close"];

/// Trend-line label for the retest prep.
pub const RETEST_LABELS: &[&str] = &["retest", "neckline-retest", "retrace"];

/// Vertical-line label for the trade-expiry veto.
pub const TRADE_EXPIRY_LABELS: &[&str] = &["trade-expiry", "trade-expired"];

/// Vertical-line label for the *start* of a blackout (pause) window.
pub const BLACKOUT_START_LABELS: &[&str] = &["blackout-start", "pause"];

/// Vertical-line label for the *end* of a blackout (pause) window.
pub const BLACKOUT_END_LABELS: &[&str] = &["blackout-end", "resume"];

/// Vertical-line label for the start of a news window.
pub const NEWS_START_LABELS: &[&str] = &["news-start"];

/// Vertical-line label for the end of a news window.
pub const NEWS_END_LABELS: &[&str] = &["news-end"];

/// Horizontal-line label for the invalidation veto. `too-high` is
/// short-trade invalidation; `too-low` is long-trade invalidation.
pub const INVALIDATION_LABELS: &[&str] = &["too-high", "too-low"];

/// Case-insensitive membership test. Returns true when `label`
/// (trimmed, lowercase) matches any entry in `vocab`.
pub fn matches(label: &str, vocab: &[&str]) -> bool {
    let trimmed = label.trim();
    // ASCII-only label vocabularies — chars().eq_ignore_ascii_case
    // is fine here without an allocation.
    vocab
        .iter()
        .any(|&v| trimmed.len() == v.len() && trimmed.eq_ignore_ascii_case(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_canonical() {
        assert!(matches("neckline", BREAK_LABELS));
        assert!(matches("retest", RETEST_LABELS));
        assert!(matches("trade-expiry", TRADE_EXPIRY_LABELS));
    }

    #[test]
    fn matches_aliases() {
        assert!(matches("break-and-close", BREAK_LABELS));
        assert!(matches("retrace", RETEST_LABELS));
        assert!(matches("pause", BLACKOUT_START_LABELS));
        assert!(matches("resume", BLACKOUT_END_LABELS));
    }

    #[test]
    fn case_insensitive() {
        assert!(matches("NECKLINE", BREAK_LABELS));
        assert!(matches("Too-High", INVALIDATION_LABELS));
    }

    #[test]
    fn trim_whitespace() {
        assert!(matches("  neckline  ", BREAK_LABELS));
    }

    #[test]
    fn rejects_unknown() {
        assert!(!matches("not-a-label", BREAK_LABELS));
        assert!(!matches("", BREAK_LABELS));
        assert!(!matches("necklin", BREAK_LABELS));
    }
}
