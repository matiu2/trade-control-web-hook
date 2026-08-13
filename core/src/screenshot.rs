//! Recognising a TradingView **snapshot** (screenshot) URL.
//!
//! `tv-arm register` reads the system clipboard at arm time and, if it holds a
//! TradingView snapshot link, bakes it onto the plan so the journal can show
//! the chart as the operator saw it when they armed. This module owns the one
//! question that needs judgement — *is this string such a URL?* — so the
//! clipboard plumbing (tv-arm) and the display (journal) share one answer.
//!
//! The shape TradingView mints from its camera button is:
//!
//! ```text
//! https://www.tradingview.com/x/pM2uDdC2/
//! ```
//!
//! We deliberately match **only** that snapshot shape, not any tradingview.com
//! URL: the clipboard is a shared, incidental surface. Whatever the operator
//! last copied — a chart link, a symbol page, an unrelated URL — must not be
//! mistaken for a screenshot and journalled as one. Recognition is narrow so a
//! non-match is the common, silent case rather than a false positive.

use serde::{Deserialize, Serialize};

/// A validated TradingView snapshot URL, normalised to its canonical form.
///
/// Constructing one is the only way to assert "this really is a screenshot
/// link" — [`parse`](ScreenshotUrl::parse) is the sole constructor, so an
/// unvalidated string can't reach a plan by mistake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScreenshotUrl(String);

/// The snapshot id is TradingView's short base62 slug (`pM2uDdC2`). Bounds are
/// deliberately loose — we're distinguishing a snapshot link from arbitrary
/// clipboard junk, not validating TradingView's id scheme.
const MIN_ID_LEN: usize = 4;
const MAX_ID_LEN: usize = 32;

impl ScreenshotUrl {
    /// Recognise a TradingView snapshot URL in `raw`, returning `None` for
    /// anything else. Surrounding whitespace is trimmed (a clipboard copy
    /// commonly carries a trailing newline) and a missing trailing slash is
    /// tolerated, but the host and `/x/` path are required.
    ///
    /// The result is normalised to `https://www.tradingview.com/x/<id>/` so two
    /// copies of the same snapshot that differ only in scheme, `www.`, or the
    /// trailing slash compare equal.
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        // Reject anything with internal whitespace up front: a clipboard often
        // holds a block of prose that happens to *contain* a link, and baking a
        // whole paragraph onto the plan would be worse than baking nothing.
        if trimmed.is_empty() || trimmed.split_whitespace().count() != 1 {
            return None;
        }
        let rest = strip_scheme(trimmed)?;
        let rest = rest.strip_prefix("www.").unwrap_or(rest);
        let path = rest.strip_prefix("tradingview.com/x/")?;
        let id = path.strip_suffix('/').unwrap_or(path);
        if !is_snapshot_id(id) {
            return None;
        }
        Some(Self(format!("https://www.tradingview.com/x/{id}/")))
    }

    /// The canonical URL, ready to print or open.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ScreenshotUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Accept either scheme; a bare `www.tradingview.com/x/…` paste is also fine.
fn strip_scheme(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix("https://") {
        return Some(rest);
    }
    if let Some(rest) = s.strip_prefix("http://") {
        return Some(rest);
    }
    // No scheme at all — only allow it when it still looks like the host, so a
    // random word can't fall through to the path check.
    if s.starts_with("www.tradingview.com/") || s.starts_with("tradingview.com/") {
        return Some(s);
    }
    None
}

/// A snapshot id is a non-empty run of base62 characters of plausible length.
/// Anything with a further path segment, query, or fragment fails here — those
/// carry a `/`, `?`, or `#`, none of which are alphanumeric.
fn is_snapshot_id(id: &str) -> bool {
    (MIN_ID_LEN..=MAX_ID_LEN).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape TradingView's camera button puts on the clipboard.
    #[test]
    fn parses_the_canonical_snapshot_url() {
        let url = ScreenshotUrl::parse("https://www.tradingview.com/x/pM2uDdC2/")
            .expect("canonical snapshot URL should parse");
        assert_eq!(url.as_str(), "https://www.tradingview.com/x/pM2uDdC2/");
    }

    /// A clipboard copy usually carries a trailing newline; it must not defeat
    /// recognition.
    #[test]
    fn trims_surrounding_whitespace() {
        let url = ScreenshotUrl::parse("  https://www.tradingview.com/x/pM2uDdC2/\n")
            .expect("whitespace-padded URL should parse");
        assert_eq!(url.as_str(), "https://www.tradingview.com/x/pM2uDdC2/");
    }

    /// Scheme, `www.`, and the trailing slash are all normalised away, so the
    /// same snapshot copied from different places compares equal.
    #[test]
    fn normalises_scheme_host_and_trailing_slash() {
        let canonical = "https://www.tradingview.com/x/pM2uDdC2/";
        for variant in [
            "http://www.tradingview.com/x/pM2uDdC2/",
            "https://tradingview.com/x/pM2uDdC2",
            "www.tradingview.com/x/pM2uDdC2/",
            "tradingview.com/x/pM2uDdC2",
        ] {
            let url = ScreenshotUrl::parse(variant)
                .unwrap_or_else(|| panic!("{variant} should parse as a snapshot URL"));
            assert_eq!(url.as_str(), canonical, "variant {variant} normalised");
        }
    }

    /// The clipboard is a shared surface. Anything that isn't a snapshot link —
    /// including *other* TradingView URLs — must be ignored, not journalled.
    #[test]
    fn rejects_non_snapshot_clipboard_contents() {
        for raw in [
            "",
            "   ",
            "hello world",
            "EUR_USD",
            // A TradingView chart link is not a screenshot.
            "https://www.tradingview.com/chart/pM2uDdC2/",
            // The symbol page is not a screenshot.
            "https://www.tradingview.com/symbols/EURUSD/",
            // Right path shape, wrong host.
            "https://example.com/x/pM2uDdC2/",
            // Lookalike host must not pass the `www.`/bare-host check.
            "https://nottradingview.com/x/pM2uDdC2/",
            // Empty id.
            "https://www.tradingview.com/x/",
            // Deeper path than a snapshot id.
            "https://www.tradingview.com/x/pM2uDdC2/extra/",
            // Query/fragment are not part of the snapshot shape.
            "https://www.tradingview.com/x/pM2uDdC2?foo=1",
            "https://www.tradingview.com/x/pM2uDdC2#frag",
        ] {
            assert!(
                ScreenshotUrl::parse(raw).is_none(),
                "{raw:?} must not be taken for a snapshot URL"
            );
        }
    }

    /// A paragraph that merely *contains* a link is not a copied screenshot —
    /// baking the whole block onto the plan would be worse than baking nothing.
    #[test]
    fn rejects_prose_that_merely_contains_a_link() {
        let raw = "look at https://www.tradingview.com/x/pM2uDdC2/ for the setup";
        assert!(ScreenshotUrl::parse(raw).is_none());
    }

    /// Ids far outside the plausible length band are junk, not snapshots.
    #[test]
    fn rejects_implausible_id_lengths() {
        assert!(ScreenshotUrl::parse("https://www.tradingview.com/x/ab/").is_none());
        let long = "a".repeat(MAX_ID_LEN + 1);
        assert!(ScreenshotUrl::parse(&format!("https://www.tradingview.com/x/{long}/")).is_none());
    }

    /// Serialises as a bare JSON string (`#[serde(transparent)]`), so the plan
    /// body stays readable and a hand-edited plan round-trips.
    #[test]
    fn round_trips_as_a_bare_json_string() {
        let url = ScreenshotUrl::parse("https://www.tradingview.com/x/pM2uDdC2/")
            .expect("URL should parse");
        let json = serde_json::to_string(&url).expect("serialise");
        assert_eq!(json, "\"https://www.tradingview.com/x/pM2uDdC2/\"");
        let back: ScreenshotUrl = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, url);
    }
}
