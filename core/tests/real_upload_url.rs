//! The URLs from the real upload-and-fetch-back proofs must parse.
//!
//! These are not synthetic fixtures: each was minted by an actual
//! `POST https://catbox.moe/user/api.php`, fetched back, and confirmed
//! byte-identical (sha256) to the chart PNG that was captured. They are pinned
//! here so a future narrowing of `parse` that would reject a genuine upload
//! fails loudly.
use trade_control_core::screenshot::ScreenshotUrl;

#[test]
fn real_catbox_upload_urls_parse_and_round_trip() {
    for raw in [
        // Direct API upload of the captured chart (170,634 bytes).
        "https://files.catbox.moe/ogsh5n.png",
        // End-to-end through local-chart's camera button (170,806 bytes).
        "https://files.catbox.moe/d3pm9u.png",
    ] {
        let url = ScreenshotUrl::parse(raw)
            .unwrap_or_else(|| panic!("{raw} is a real upload and must parse"));
        assert_eq!(url.as_str(), raw, "already canonical, so unchanged");

        let json = serde_json::to_string(&url).expect("serialise");
        let back: ScreenshotUrl = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, url);
    }
}
