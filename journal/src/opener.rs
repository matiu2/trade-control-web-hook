//! Open a URL in the operator's browser.
//!
//! Shelling out to `xdg-open` rather than linking a crate, for the same reason
//! [`crate::clipboard`] shells out: the desktop tool already knows the user's
//! configured browser, and it keeps the TUI free of a dependency for a
//! one-line job.
//!
//! The child is **detached** — stdio nulled and never waited on. A browser
//! launcher can take seconds to return (and may not return at all while the
//! browser lives), so waiting would freeze the TUI's event loop mid-frame.

use std::process::{Command, Stdio};

use color_eyre::eyre::{Result, eyre};

/// URL openers, tried in order. `xdg-open` is the freedesktop standard;
/// `gio open` covers GNOME installs where `xdg-utils` is absent.
const OPENERS: &[(&str, &[&str])] = &[("xdg-open", &[]), ("gio", &["open"])];

/// Open `url` in the default browser, returning the tool that accepted it.
///
/// Success here means "the launcher started", not "the page loaded" — we
/// deliberately don't wait for the child, so a browser that fails *after*
/// launch can't be detected. That's the right trade: the alternative blocks
/// the UI.
pub fn open(url: &str) -> Result<&'static str> {
    let mut last_err = String::new();
    for (tool, args) in OPENERS {
        match spawn_detached(tool, args, url) {
            Ok(()) => return Ok(tool),
            Err(e) => last_err = format!("{tool}: {e}"),
        }
    }
    Err(eyre!(
        "no URL opener worked (tried xdg-open, gio) — last: {last_err}"
    ))
}

/// Spawn `tool args url` with stdio detached, without waiting for it.
fn spawn_detached(tool: &str, args: &[&str], url: &str) -> Result<()> {
    Command::new(tool)
        .args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| eyre!("spawn: {e}"))?;
    Ok(())
}
