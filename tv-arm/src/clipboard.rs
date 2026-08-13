//! Arm-time capture of a TradingView screenshot URL from the system clipboard.
//!
//! Workflow this serves: the operator presses TradingView's camera button
//! ("Copy link to the chart image"), which puts a snapshot URL like
//! `https://www.tradingview.com/x/pM2uDdC2/` on the clipboard, then runs
//! `tv-arm register`. We read the clipboard, and if it holds such a URL we bake
//! it onto the plan so the journal can show the chart exactly as it looked when
//! the trade was armed.
//!
//! Everything here is **fail-soft**, the same discipline as the arm-time
//! sentiment snapshot ([`crate::sentiment`]): no clipboard tool installed, an
//! empty clipboard, or contents that aren't a snapshot URL all yield `None` and
//! let arming continue. A screenshot is a journalling nicety; arming is the
//! critical path and must never depend on what happens to be on the clipboard.
//!
//! We shell out to a clipboard CLI rather than link a crate, mirroring
//! `journal/src/clipboard.rs` (which explains the Wayland ownership reason for
//! the write direction). For *reading* the practical argument is the same one:
//! `wl-paste` talks to the compositor that actually owns the selection, and it
//! keeps `tv-arm` free of an X11/Wayland dependency it otherwise doesn't need.

use std::process::Command;

use tracing::{debug, info};
use trade_control_core::screenshot::ScreenshotUrl;

/// Clipboard readers, tried in order: `wl-paste` for Wayland, then the X11
/// pair. `-n` / `-o` make each print the selection to stdout without a trailing
/// newline fuss (we trim anyway).
const TOOLS: &[(&str, &[&str])] = &[
    ("wl-paste", &["-n"]),
    ("xclip", &["-selection", "clipboard", "-o"]),
    ("xsel", &["--clipboard", "--output"]),
];

/// Read the system clipboard and return a TradingView snapshot URL if that's
/// what it holds. `None` for anything else — no clipboard tool available, an
/// empty clipboard, or contents that aren't a snapshot link.
///
/// Logs at `info` when a URL is captured (so the operator can see it landed on
/// the plan) and at `debug` otherwise, since "clipboard held something else" is
/// the ordinary case and must not look like a problem.
pub fn screenshot_url_from_clipboard() -> Option<ScreenshotUrl> {
    let raw = read_clipboard()?;
    match ScreenshotUrl::parse(&raw) {
        Some(url) => {
            info!(url = %url, "captured TradingView screenshot URL from clipboard");
            Some(url)
        }
        None => {
            debug!("clipboard holds no TradingView screenshot URL; none baked onto the plan");
            None
        }
    }
}

/// Read the clipboard with the first tool that works. `None` if every tool is
/// missing or fails — indistinguishable, and equally uninteresting, from an
/// empty clipboard.
fn read_clipboard() -> Option<String> {
    for (tool, args) in TOOLS {
        match Command::new(tool).args(*args).output() {
            Ok(out) if out.status.success() => {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
            // A present-but-failing tool (e.g. `wl-paste` on an X11 session)
            // is normal — fall through to the next one.
            Ok(out) => debug!(tool, status = %out.status, "clipboard read failed; trying next"),
            Err(e) => debug!(tool, error = %e, "clipboard tool unavailable; trying next"),
        }
    }
    debug!("no clipboard tool available (tried wl-paste, xclip, xsel)");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capture is only as good as the recogniser, which is tested in
    /// `core::screenshot`. What this module owns is the *fail-soft* contract:
    /// clipboard contents that aren't a snapshot URL must yield `None` rather
    /// than an error or a junk value baked onto the plan.
    #[test]
    fn only_a_snapshot_url_is_captured() {
        assert!(ScreenshotUrl::parse("https://www.tradingview.com/x/pM2uDdC2/").is_some());
        for junk in [
            "",
            "some copied text",
            "https://www.tradingview.com/chart/abc/",
        ] {
            assert!(
                ScreenshotUrl::parse(junk).is_none(),
                "{junk:?} must not be captured"
            );
        }
    }

    /// Reading must never panic or block regardless of what (if anything) is on
    /// this machine's clipboard — the whole point of the fail-soft contract.
    /// Whatever it returns, it returns without unwinding.
    #[test]
    fn reading_the_clipboard_never_panics() {
        screenshot_url_from_clipboard();
    }
}
