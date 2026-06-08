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

/// Horizontal-line labels for support / resistance levels — each
/// becomes a price band for the `07-close-on-sr-reversal` alert.
pub const SR_LEVEL_LABELS: &[&str] = &["support", "resistance"];

/// Suffix that turns a prep label into a prep-expiry vertical-line
/// label: `break-and-close` → `break-and-close-expiry`. The chart
/// reader strips this suffix and matches the remainder against the
/// prep vocabularies to find which prep the line clamps.
pub const PREP_EXPIRY_SUFFIX: &str = "-expiry";

/// Canonical prep *step* names, as they appear in an enter intent's
/// `requires_preps` gate and a prep alert's `step` field. These are
/// the names the worker keys prep state on — distinct from the
/// drawing-label vocabularies (which carry operator-friendly aliases
/// like `neckline` / `retrace`).
pub const PREP_BREAK_AND_CLOSE: &str = "break-and-close";
pub const PREP_RETEST: &str = "retest";

/// Resolve a `<prep>-expiry` vertical-line label to the canonical prep
/// step name it clamps. Strips [`PREP_EXPIRY_SUFFIX`] and matches the
/// remainder (case-insensitively) against the prep vocabularies:
///
/// - `break-and-close-expiry` / `neckline-expiry` → `break-and-close`
/// - `retest-expiry` / `retrace-expiry` → `retest`
///
/// Returns `None` when the label doesn't end in `-expiry`, or the
/// stem isn't a recognised prep. Note that `trade-expiry` resolves to
/// `None` here — `trade` is not a prep, so the dedicated trade-expiry
/// veto path owns it and there's no collision.
pub fn prep_name_from_expiry_label(label: &str) -> Option<&'static str> {
    let trimmed = label.trim();
    // Match the suffix case-insensitively, then take the stem before it.
    if trimmed.len() <= PREP_EXPIRY_SUFFIX.len() {
        return None;
    }
    let (stem, suffix) = trimmed.split_at(trimmed.len() - PREP_EXPIRY_SUFFIX.len());
    if !suffix.eq_ignore_ascii_case(PREP_EXPIRY_SUFFIX) {
        return None;
    }
    if matches(stem, BREAK_LABELS) {
        Some(PREP_BREAK_AND_CLOSE)
    } else if matches(stem, RETEST_LABELS) {
        Some(PREP_RETEST)
    } else {
        None
    }
}

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

    #[test]
    fn prep_expiry_resolves_break_and_close() {
        assert_eq!(
            prep_name_from_expiry_label("break-and-close-expiry"),
            Some(PREP_BREAK_AND_CLOSE)
        );
        // The `neckline` alias for break-and-close also resolves.
        assert_eq!(
            prep_name_from_expiry_label("neckline-expiry"),
            Some(PREP_BREAK_AND_CLOSE)
        );
    }

    #[test]
    fn prep_expiry_resolves_retest() {
        assert_eq!(
            prep_name_from_expiry_label("retest-expiry"),
            Some(PREP_RETEST)
        );
        assert_eq!(
            prep_name_from_expiry_label("retrace-expiry"),
            Some(PREP_RETEST)
        );
    }

    #[test]
    fn prep_expiry_is_case_and_whitespace_insensitive() {
        assert_eq!(
            prep_name_from_expiry_label("  Break-And-Close-EXPIRY  "),
            Some(PREP_BREAK_AND_CLOSE)
        );
    }

    #[test]
    fn prep_expiry_rejects_non_prep_stems() {
        // `trade-expiry` is the dedicated whole-trade veto, not a prep —
        // it must NOT resolve here (no collision with the prep path).
        assert_eq!(prep_name_from_expiry_label("trade-expiry"), None);
        assert_eq!(prep_name_from_expiry_label("nonsense-expiry"), None);
    }

    #[test]
    fn prep_expiry_rejects_missing_suffix() {
        assert_eq!(prep_name_from_expiry_label("break-and-close"), None);
        assert_eq!(prep_name_from_expiry_label("retest"), None);
        assert_eq!(prep_name_from_expiry_label(""), None);
        assert_eq!(prep_name_from_expiry_label("-expiry"), None);
    }
}
