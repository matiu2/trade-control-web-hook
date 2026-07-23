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

/// Run `replay-candles-<env> --plan <file>`. When `annotate` is set, also draw
/// the simulated positions onto the live TradingView chart via tv-mcp. Returns
/// the replay report (stdout); stderr is appended on failure.
pub fn replay(plan_file: &std::path::Path, annotate: bool) -> Result<String> {
    let program = bin("replay-candles");
    let mut cmd = Command::new(&program);
    cmd.arg("--plan").arg(plan_file);
    if annotate {
        cmd.arg("--annotate").arg("true");
    }
    let out = cmd
        .output()
        .map_err(|e| eyre!("failed to launch `{program}`: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(eyre!(
            "`{program} --plan {}` failed ({}): {}\n{stdout}",
            plan_file.display(),
            out.status,
            stderr.trim()
        ));
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

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
