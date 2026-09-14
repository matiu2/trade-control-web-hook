//! Recognising a chart **snapshot** (screenshot) URL.
//!
//! `tv-arm register` reads the system clipboard at arm time and, if it holds a
//! snapshot link, bakes it onto the plan so the journal can show the chart as
//! the operator saw it when they armed. This module owns the one question that
//! needs judgement — *is this string such a URL?* — so the clipboard plumbing
//! (tv-arm) and the display (journal) share one answer.
//!
//! Two shapes are recognised, one per source of screenshots:
//!
//! ```text
//! https://www.tradingview.com/x/pM2uDdC2/     TradingView's camera button
//! https://files.catbox.moe/ogsh5n.png         local-chart's own capture
//! ```
//!
//! The TradingView shape is **historical but still load-bearing**: plans armed
//! before local-chart existed carry those links, and they must keep parsing.
//! The Catbox shape is where new screenshots go now that the TradingView
//! subscription is gone — local-chart captures its own chart, uploads it, and
//! puts the URL on the clipboard, so this one module is the whole downstream
//! change.
//!
//! **Each host is matched as a SPECIFIC shape — never "any URL".** The
//! clipboard is a shared, incidental surface. Whatever the operator last
//! copied — a chart link, a symbol page, a password, an unrelated URL — must
//! not be mistaken for a screenshot and journalled as one. Recognition stays
//! narrow so a non-match is the common, silent case rather than a false
//! positive. Adding a host means adding another exact host+path+extension
//! recogniser here, not loosening the existing ones.

use serde::{Deserialize, Serialize};

/// A validated chart snapshot URL, normalised to its canonical form.
///
/// Constructing one is the only way to assert "this really is a screenshot
/// link" — [`parse`](ScreenshotUrl::parse) is the sole constructor, so an
/// unvalidated string can't reach a plan by mistake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScreenshotUrl(String);

/// The snapshot id is TradingView's short base62 slug (`pM2uDdC2`), or
/// Catbox's short alphanumeric filename stem (`ogsh5n`). Bounds are
/// deliberately loose — we're distinguishing a snapshot link from arbitrary
/// clipboard junk, not validating either host's id scheme.
const MIN_ID_LEN: usize = 4;
const MAX_ID_LEN: usize = 32;

/// Catbox serves uploads from this one host. A file lives at the root, so the
/// path is exactly `/<stem>.<ext>` — no directories, no query.
const CATBOX_HOST: &str = "files.catbox.moe";

/// Image extensions local-chart can upload. Restricting these is part of
/// keeping the match narrow: a `.zip` or `.txt` on Catbox is not a screenshot,
/// so it must not be journalled as one.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];

impl ScreenshotUrl {
    /// Recognise a snapshot URL in `raw`, returning `None` for anything else.
    /// Surrounding whitespace is trimmed (a clipboard copy commonly carries a
    /// trailing newline).
    ///
    /// Two host shapes are accepted, each matched exactly — see the module
    /// docs. The result is normalised per host so two copies of the same
    /// snapshot that differ only in scheme, `www.`, or the trailing slash
    /// compare equal.
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        // Reject anything with internal whitespace up front: a clipboard often
        // holds a block of prose that happens to *contain* a link, and baking a
        // whole paragraph onto the plan would be worse than baking nothing.
        if trimmed.is_empty() || trimmed.split_whitespace().count() != 1 {
            return None;
        }
        let rest = strip_scheme(trimmed)?;
        // Ordered, and each arm is a complete host+path match, so the two
        // recognisers cannot shadow one another the way an untagged serde
        // superset can. A `None` from one is not a licence for the other to
        // relax — it just tries its own exact shape.
        parse_tradingview(rest).or_else(|| parse_catbox(rest))
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

/// Accept either scheme; a bare `www.tradingview.com/x/…` or
/// `files.catbox.moe/…` paste is also fine.
fn strip_scheme(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix("https://") {
        return Some(rest);
    }
    if let Some(rest) = s.strip_prefix("http://") {
        return Some(rest);
    }
    // No scheme at all — only allow it when it still looks like one of the
    // known hosts, so a random word can't fall through to the path checks.
    const BARE_HOSTS: &[&str] = &["www.tradingview.com/", "tradingview.com/"];
    if BARE_HOSTS.iter().any(|h| s.starts_with(h)) {
        return Some(s);
    }
    if s.starts_with(&format!("{CATBOX_HOST}/")) {
        return Some(s);
    }
    None
}

/// The TradingView camera-button shape: `www.tradingview.com/x/<id>/`, with
/// `www.` and the trailing slash both optional. Scheme already stripped.
///
/// Normalises to `https://www.tradingview.com/x/<id>/`.
fn parse_tradingview(rest: &str) -> Option<ScreenshotUrl> {
    let host_stripped = rest.strip_prefix("www.").unwrap_or(rest);
    let path = host_stripped.strip_prefix("tradingview.com/x/")?;
    let id = path.strip_suffix('/').unwrap_or(path);
    if !is_snapshot_id(id) {
        return None;
    }
    Some(ScreenshotUrl(format!(
        "https://www.tradingview.com/x/{id}/"
    )))
}

/// The Catbox shape local-chart uploads to: `files.catbox.moe/<stem>.<ext>`,
/// where `ext` is an image extension. Scheme already stripped.
///
/// A file sits at the host root, so exactly one path segment is allowed —
/// that, plus the extension check, is what keeps this from matching arbitrary
/// Catbox links. There is deliberately **no** trailing-slash tolerance: a real
/// Catbox image URL never has one, and accepting it would invent a second
/// spelling of the same URL for no gain.
///
/// Normalises to `https://files.catbox.moe/<stem>.<ext>` with the extension
/// lowercased, so `.PNG` and `.png` compare equal.
fn parse_catbox(rest: &str) -> Option<ScreenshotUrl> {
    let path = rest.strip_prefix(CATBOX_HOST)?.strip_prefix('/')?;
    // One segment only: no directories, no query, no fragment.
    if path.contains('/') || path.contains('?') || path.contains('#') {
        return None;
    }
    let (stem, ext) = path.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    if !IMAGE_EXTS.contains(&ext.as_str()) || !is_snapshot_id(stem) {
        return None;
    }
    Some(ScreenshotUrl(format!("https://{CATBOX_HOST}/{stem}.{ext}")))
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

    /// The exact shape local-chart's upload returns, as Catbox's API prints it.
    /// This is the real URL from the upload-and-fetch-back proof.
    #[test]
    fn parses_the_canonical_catbox_url() {
        let url = ScreenshotUrl::parse("https://files.catbox.moe/ogsh5n.png")
            .expect("canonical catbox URL should parse");
        assert_eq!(url.as_str(), "https://files.catbox.moe/ogsh5n.png");
    }

    /// Scheme and extension case are normalised away, so the same upload
    /// copied from different places compares equal. Note there is no
    /// trailing-slash variant: a Catbox image URL never carries one.
    #[test]
    fn normalises_catbox_scheme_and_extension_case() {
        let canonical = "https://files.catbox.moe/ogsh5n.png";
        for variant in [
            "http://files.catbox.moe/ogsh5n.png",
            "files.catbox.moe/ogsh5n.png",
            "https://files.catbox.moe/ogsh5n.PNG",
            "  https://files.catbox.moe/ogsh5n.png\n",
        ] {
            let url = ScreenshotUrl::parse(variant)
                .unwrap_or_else(|| panic!("{variant} should parse as a snapshot URL"));
            assert_eq!(url.as_str(), canonical, "variant {variant} normalised");
        }
    }

    /// Every image extension local-chart might upload is recognised.
    #[test]
    fn parses_each_supported_image_extension() {
        for ext in IMAGE_EXTS {
            let raw = format!("https://files.catbox.moe/ogsh5n.{ext}");
            assert!(
                ScreenshotUrl::parse(&raw).is_some(),
                "{raw} should parse as a snapshot URL"
            );
        }
    }

    /// The Catbox arm is a SPECIFIC shape, not "any catbox.moe URL" and
    /// certainly not "any URL". Each case here is a real way the narrowness
    /// could be lost — a wrong host, a non-image file, a nested path, a query.
    #[test]
    fn rejects_catbox_lookalikes_and_non_images() {
        for raw in [
            // Right path shape, wrong host — the litterbox sibling is the
            // TEMPORARY one (files expire), so it must never be journalled.
            "https://litter.catbox.moe/ogsh5n.png",
            "https://litterbox.catbox.moe/ogsh5n.png",
            // The site itself, not a file.
            "https://catbox.moe/ogsh5n.png",
            "https://catbox.moe/",
            // Lookalike host must not pass the bare-host check.
            "https://notfiles.catbox.moe/ogsh5n.png",
            "https://files.catbox.moe.evil.com/ogsh5n.png",
            // Not an image: a Catbox account can hold any file type.
            "https://files.catbox.moe/ogsh5n.zip",
            "https://files.catbox.moe/ogsh5n.txt",
            "https://files.catbox.moe/ogsh5n.mp4",
            // No extension at all.
            "https://files.catbox.moe/ogsh5n",
            // Nested path, query, fragment — not the flat file shape.
            "https://files.catbox.moe/dir/ogsh5n.png",
            "https://files.catbox.moe/ogsh5n.png?raw=1",
            "https://files.catbox.moe/ogsh5n.png#frag",
            // A trailing slash is not a real Catbox image URL.
            "https://files.catbox.moe/ogsh5n.png/",
            // Empty / implausible stems.
            "https://files.catbox.moe/.png",
            "https://files.catbox.moe/ab.png",
        ] {
            assert!(
                ScreenshotUrl::parse(raw).is_none(),
                "{raw:?} must not be taken for a snapshot URL"
            );
        }
    }

    /// The whole point of the narrowness, stated as one test: ordinary
    /// clipboard contents must not parse under EITHER host arm. If a mutation
    /// widens `parse` to accept any URL, this is what goes red.
    #[test]
    fn rejects_ordinary_clipboard_contents_under_both_hosts() {
        for raw in [
            "https://example.com/screenshot.png",
            "https://imgur.com/a/abc123",
            "https://github.com/matiu2/trading-libraries",
            "https://files.catbox.example/ogsh5n.png",
            "correct horse battery staple",
            "/home/matiu/chart.png",
            "EUR_USD h4 short",
        ] {
            assert!(
                ScreenshotUrl::parse(raw).is_none(),
                "{raw:?} must not be taken for a snapshot URL"
            );
        }
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
