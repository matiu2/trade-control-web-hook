//! Subprocess wrappers around the environment-suffixed `trade-control-<env>`
//! and `replay-candles-<env>` CLIs. **This is the only module that knows the
//! CLI invocation shapes.** The wider `tv-arm` client surface is mid-refactor
//! (flags → subcommands, another agent), so keeping every shell-out isolated
//! here means a later flag/subcommand flip is a one-line change per function.
//!
//! Pinned shapes (verified 2026-07-23 against `--help`):
//!
//! * `trade-control-<env> plan list --include-all --yaml --key-file <K>`
//!   → YAML sequence of per-plan summaries.
//! * `trade-control-<env> plan timeline <ID> --json --key-file <K>`
//!   → `{records, ticks}` JSON (`trade_control_core::recording::PlanTimeline`).
//! * `trade-control-<env> plan export <ID> --key-file <K>`
//!   → single-line flow JSON of the bare `TradePlan` (re-registerable).
//! * `trade-control-<env> plan delete <ID> --key-file <K>`
//!   → deletes plan + engine state (idempotent).
//! * `replay-candles-<env> --plan <FILE> [--annotate true]`
//!   → replay report on stdout; `--annotate` also draws it on the live TV chart.

use std::path::PathBuf;
use std::process::Command;

use color_eyre::eyre::{Result, eyre};

/// This environment's CLI suffix, baked at compile time (`dev` / `staging`,
/// empty for a plain `cargo build`). See `build.rs`.
const ENV_SUFFIX: &str = env!("BAKED_ENV_SUFFIX");

/// Resolve `trade-control` / `replay-candles` to the suffixed binary for this
/// environment. Empty suffix → the bare name on `PATH`.
fn bin(base: &str) -> String {
    if ENV_SUFFIX.is_empty() {
        base.to_string()
    } else {
        format!("{base}-{ENV_SUFFIX}")
    }
}

/// The signing key file. Honours `TRADE_CONTROL_KEY_FILE` (same env var the
/// stock CLIs read) and otherwise defaults to the conventional location.
fn key_file() -> PathBuf {
    if let Ok(p) = std::env::var("TRADE_CONTROL_KEY_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/trade-control/key.hex")
}

/// Run a `trade-control-<env>` subcommand, returning its stdout on success.
/// A non-zero exit surfaces the CLI's stderr verbatim (load-bearing: a 404 for
/// a missing plan, a signing error, etc.) rather than a bare status code.
fn run_trade_control(args: &[&str]) -> Result<String> {
    let key = key_file();
    let program = bin("trade-control");
    let mut cmd = Command::new(&program);
    cmd.args(args).arg("--key-file").arg(&key);
    let out = cmd
        .output()
        .map_err(|e| eyre!("failed to launch `{program}`: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(eyre!(
            "`{program} {}` failed ({}): {}",
            args.join(" "),
            out.status,
            stderr.trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| eyre!("`{program}` stdout was not UTF-8: {e}"))
}

/// `plan list --include-all --yaml` → raw YAML sequence of plan summaries.
pub fn plan_list_yaml() -> Result<String> {
    run_trade_control(&["plan", "list", "--include-all", "--yaml"])
}

/// `plan timeline <id> --json` → raw `PlanTimeline` JSON.
pub fn plan_timeline_json(trade_id: &str) -> Result<String> {
    run_trade_control(&["plan", "timeline", trade_id, "--json"])
}

/// `plan export <id>` → single-line flow JSON of the bare `TradePlan`.
pub fn plan_export_json(trade_id: &str) -> Result<String> {
    run_trade_control(&["plan", "export", trade_id])
}

/// `plan delete <id>` → deletes the plan + engine state. Idempotent.
pub fn plan_delete(trade_id: &str) -> Result<String> {
    run_trade_control(&["plan", "delete", trade_id])
}

/// Replay the setup by re-arming it from the **live TradingView chart** and
/// chaining into `replay-candles`, via `tv-arm-<env> --start <armed_at> replay`.
///
/// The journal has already loaded the plan's chart (symbol + timeframe, right
/// broker), so tv-arm reads the instrument, timeframe, and **broker from the
/// chart's own exchange** — no `--instrument`/`--source` to pass, and no
/// instrument-resolution failure for OANDA-only assets (e.g. the XAU/XAG ratio
/// that isn't listed on TradeNation). `--start <armed_at>` is the "live now"
/// cursor: tv-arm walks the whole chart to find the pattern's roles
/// (neckline / invalidation / expiry) relative to it.
///
/// `armed_at` is the plan's RFC3339 UTC arm time. The `replay` subcommand
/// defaults to `--verbose --annotate true --source <chart-broker>`; we take
/// those defaults (annotate draws the sim onto the chart, which is fine — the
/// chart is the focus). Returns the replay report (stdout) with **ANSI escape
/// sequences stripped** — `--verbose` colours its tracing, and raw `\x1b[…m`
/// codes embedded in the text corrupt the ratatui render (they're drawn as
/// literal glyphs, not interpreted as colour). Stripping at the source keeps
/// both the report view and the divergence parser on clean text. Stderr is
/// appended on failure.
pub fn replay_via_tv_arm(armed_at: &str) -> Result<String> {
    let program = bin("tv-arm");
    let mut cmd = Command::new(&program);
    cmd.arg("--start").arg(armed_at).arg("replay");
    // tv-arm logs its pipeline at INFO on **stdout** (mixed into the report we
    // capture); quiet it to warn so the report body dominates. Honour an
    // operator's own RUST_LOG if they set one. (ANSI is stripped regardless.)
    if std::env::var_os("RUST_LOG").is_none() {
        cmd.env("RUST_LOG", "warn");
    }
    let out = cmd
        .output()
        .map_err(|e| eyre!("failed to launch `{program}`: {e}"))?;
    let stdout = strip_ansi(&String::from_utf8_lossy(&out.stdout));
    if !out.status.success() {
        let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
        return Err(eyre!(
            "`{program} --start {armed_at} replay` failed ({}): {}\n{stdout}",
            out.status,
            stderr.trim()
        ));
    }
    Ok(stdout)
}

/// Remove ANSI escape sequences (`ESC [ … <final>`, and lone `ESC …`) from `s`.
/// Handles the CSI sequences tracing emits for colour (`\x1b[32m`, `\x1b[0m`,
/// …); a bare `ESC` not starting a CSI is dropped with its next byte. Keeps all
/// other characters, so the report's text and layout survive intact.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // ESC. A CSI sequence is `ESC [ <params/intermediates> <final 0x40..0x7e>`.
        // A lone ESC (or ESC + a non-CSI byte) just drops the pair.
        if let Some('[') = chars.next() {
            // Consume until the final byte in 0x40..=0x7e (e.g. 'm', 'K', 'H').
            for f in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&f) {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_colour_codes() {
        // `--verbose` tracing colours its output; the raw codes corrupt the TUI.
        let raw = "\u{1b}[32m INFO\u{1b}[0m replay: 4 fires\u{1b}[1;31mSL\u{1b}[0m";
        assert_eq!(strip_ansi(raw), " INFO replay: 4 firesSL");
        // Plain text is untouched, including newlines and the report's box glyphs.
        let plain = "Plan foo (X, H1) — 4 fire(s)\n│ Live │ Replay │\n";
        assert_eq!(strip_ansi(plain), plain);
        // A lone ESC (not a CSI) is dropped with its follower, not left dangling.
        assert_eq!(strip_ansi("a\u{1b}Zb"), "ab");
    }

    #[test]
    fn bin_uses_suffix_when_present() {
        // The baked suffix is empty in a plain `cargo test`, so this asserts the
        // fallback path; the suffixed path is exercised by the deploy build.
        assert_eq!(
            bin("trade-control"),
            format!(
                "trade-control{}",
                if ENV_SUFFIX.is_empty() {
                    String::new()
                } else {
                    format!("-{ENV_SUFFIX}")
                }
            )
        );
    }
}
