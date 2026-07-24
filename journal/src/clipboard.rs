//! Copy text to the system clipboard from inside the TUI.
//!
//! We shell out to a clipboard CLI rather than link a clipboard crate: on
//! Wayland (this box is Hyprland) the copy must **outlive** our process, which
//! `wl-copy` handles by forking a tiny daemon that owns the selection — an
//! in-process clipboard (e.g. `arboard`) loses the contents the moment the TUI
//! exits. We prefer `wl-copy`, then fall back to `xclip` / `xsel` for X11.

use std::io::Write;
use std::process::{Command, Stdio};

use color_eyre::eyre::{Result, eyre};

/// A clipboard tool and the argv that makes it read the selection from stdin.
const TOOLS: &[(&str, &[&str])] = &[
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
];

/// Copy `text` to the system clipboard, trying each known tool until one works.
/// Returns the tool name that succeeded (for the status line), or an error
/// listing what was tried if none are installed / all failed.
pub fn copy(text: &str) -> Result<&'static str> {
    let mut last_err = String::new();
    for (tool, args) in TOOLS {
        match pipe_to(tool, args, text) {
            Ok(()) => return Ok(tool),
            Err(e) => last_err = format!("{tool}: {e}"),
        }
    }
    Err(eyre!(
        "no clipboard tool worked (tried wl-copy, xclip, xsel) — last: {last_err}"
    ))
}

/// Spawn `tool args`, write `text` to its stdin, and wait for success.
fn pipe_to(tool: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| eyre!("spawn: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| eyre!("no stdin handle"))?
        .write_all(text.as_bytes())
        .map_err(|e| eyre!("write: {e}"))?;
    let status = child.wait().map_err(|e| eyre!("wait: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(eyre!("exited {status}"))
    }
}
