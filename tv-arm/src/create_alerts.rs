//! Render the JS template against the alert payloads and shell out
//! to Node to POST them to TradingView.
//!
//! Port of `tv_arm_hs.py`'s `create_alerts()` + `TV_MCP_NODE_TEMPLATE`.
//! The JS itself can't be ported (it runs inside TV's WebSocket
//! connection via tv-mcp's `evaluate` / `evaluateAsync`); this module
//! is just the orchestration around it:
//!
//! 1. Serialize payloads as JSON to `/tmp/trade-control-arm-payloads.json`.
//! 2. Read [`assets/tv_mcp_template.js`](../../assets/tv_mcp_template.js)
//!    embedded via `include_str!`, substitute `{tv_mcp_root}` and
//!    `{payloads_path}`, write to `/tmp/trade-control-arm-create.mjs`.
//! 3. `node <script>` with a 60 s timeout; parse the JSON results
//!    array on stdout.
//!
//! Errors carry stdout + stderr tails so the operator can diagnose a
//! studyId-not-found or stateForAlert failure without re-running.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};

use crate::alert_spec::AlertPayload;

/// The JS template embedded at compile time.
const TV_MCP_NODE_TEMPLATE: &str = include_str!("../assets/tv_mcp_template.js");

/// Where the rendered payloads file lives. Matches the Python path so
/// side-by-side runs don't fight over the file (whichever ran last
/// wins; both write the same shape).
const PAYLOADS_PATH: &str = "/tmp/trade-control-arm-payloads.json";

/// Where the rendered Node script lives. Same rationale as above.
const SCRIPT_PATH: &str = "/tmp/trade-control-arm-create.mjs";

/// Node subprocess timeout. Matches the Python.
const NODE_TIMEOUT: Duration = Duration::from_secs(60);

/// One create-alert result as the JS template emits it. Some fields
/// are absent on failure paths (`status`/`body` missing when the JS
/// reports an `error` instead). The orchestrator prints these
/// verbatim, so we don't need a per-variant enum here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertResult {
    /// Manifest entry filename — echoed from the input so failures
    /// are attributable.
    #[serde(default)]
    pub name: Option<String>,
    /// HTTP status of the TV `create_alert` POST, when the JS got
    /// that far.
    #[serde(default)]
    pub status: Option<i64>,
    /// TV response body (first 2 KB), when the POST happened.
    #[serde(default)]
    pub body: Option<String>,
    /// Error message from the JS (study not found, stateForAlert
    /// failed, fetch threw, etc.).
    #[serde(default)]
    pub error: Option<String>,
    /// Debug breadcrumbs (tool, drawing_id, last condition series) —
    /// included when the create_alert POST happened.
    #[serde(default)]
    pub debug: Option<serde_json::Value>,
}

/// POST every payload through tv-mcp and return one result per
/// payload, in the same order. Empty input → empty output (no Node
/// process spawned).
pub fn create_alerts(payloads: &[AlertPayload], tv_mcp_root: &Path) -> Result<Vec<AlertResult>> {
    if payloads.is_empty() {
        return Ok(Vec::new());
    }
    let payloads_json =
        serde_json::to_string(payloads).wrap_err("failed to serialize alert payloads")?;
    let payloads_path = PathBuf::from(PAYLOADS_PATH);
    fs::write(&payloads_path, &payloads_json)
        .wrap_err_with(|| format!("failed to write {}", payloads_path.display()))?;

    let script = render_template(TV_MCP_NODE_TEMPLATE, tv_mcp_root, &payloads_path);
    let script_path = PathBuf::from(SCRIPT_PATH);
    fs::write(&script_path, &script)
        .wrap_err_with(|| format!("failed to write {}", script_path.display()))?;

    run_node(&script_path)
}

/// The TradingView alert destination (`web_hook` field), baked in at
/// build time by `build.rs` from `TRADE_CONTROL_WEBHOOK`. Each
/// per-environment binary (`tv-arm-staging`, `tv-arm-dev`, …) embeds its
/// own worker URL; a plain `cargo install` defaults to the dev URL. This
/// is the single source of truth for where armed alerts POST — there is
/// no longer a hard-coded URL in the JS template.
const BAKED_WEBHOOK: &str = env!("BAKED_WEBHOOK");

/// Substitute `{tv_mcp_root}`, `{payloads_path}` and `{web_hook}`
/// placeholders in the template. Plain string replacement — the
/// placeholders are uniquely-named (no `{foo}` collisions in the JS).
fn render_template(template: &str, tv_mcp_root: &Path, payloads_path: &Path) -> String {
    template
        .replace("{tv_mcp_root}", &tv_mcp_root.display().to_string())
        .replace("{payloads_path}", &payloads_path.display().to_string())
        .replace("{web_hook}", BAKED_WEBHOOK)
}

/// Spawn `node <script>` with a timeout. Parse stdout as a JSON array
/// of [`AlertResult`]. Non-zero exit / non-JSON output surface as
/// `Err`.
fn run_node(script_path: &Path) -> Result<Vec<AlertResult>> {
    // No std::process::Command timeout; we use spawn + wait_timeout
    // via a thread pattern. For simplicity (and matching the
    // Python's behavior on overrun) we set the timeout via a small
    // background-killer thread.
    let mut child = Command::new("node")
        .arg(script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .wrap_err("failed to spawn `node` — is Node.js installed?")?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        std::thread::sleep(NODE_TIMEOUT);
        let _ = tx.send(());
    });
    // Poll the child while honoring the timeout.
    loop {
        if let Some(status) = child.try_wait().wrap_err("failed to poll node process")? {
            let output = child
                .wait_with_output()
                .wrap_err("failed to collect node output")?;
            return parse_node_output(status.code(), &output.stdout, &output.stderr);
        }
        if rx.try_recv().is_ok() {
            let _ = child.kill();
            return Err(eyre!(
                "create-alerts node process (pid {pid}) exceeded {}s timeout",
                NODE_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn parse_node_output(
    exit_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Vec<AlertResult>> {
    let stdout_s = String::from_utf8_lossy(stdout);
    let stderr_s = String::from_utf8_lossy(stderr);
    if exit_code != Some(0) {
        return Err(eyre!(
            "create-alerts node exited {:?}: stderr={} stdout={}",
            exit_code,
            stderr_s.trim(),
            stdout_s.trim()
        ));
    }
    if !stderr_s.trim().is_empty() {
        // Surface to the operator but don't fail — node sometimes
        // logs benign warnings.
        eprintln!("---- create-alerts node stderr ----");
        eprintln!("{}", stderr_s.trim_end());
        eprintln!("---- end stderr ----");
    }
    serde_json::from_str::<Vec<AlertResult>>(&stdout_s).wrap_err_with(|| {
        format!(
            "create-alerts node returned non-JSON: {}",
            stdout_s.chars().take(400).collect::<String>()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_payloads_skips_node() {
        // No payloads = no Node spawn, no temp files written.
        let results = create_alerts(&[], Path::new("/anywhere")).expect("empty ok");
        assert!(results.is_empty());
    }

    #[test]
    fn template_substitution_replaces_placeholders() {
        let tpl = "import x from '{tv_mcp_root}/src/x.js'; const p = '{payloads_path}';";
        let out = render_template(tpl, Path::new("/tmp/root"), Path::new("/tmp/p.json"));
        assert!(out.contains("/tmp/root/src/x.js"));
        assert!(out.contains("/tmp/p.json"));
        assert!(!out.contains("{tv_mcp_root}"));
        assert!(!out.contains("{payloads_path}"));
    }

    #[test]
    fn embedded_template_has_placeholders() {
        // Sanity check the asset got embedded with the right
        // placeholders — catches accidental drift in the template
        // file when reviewers don't run the binary.
        assert!(TV_MCP_NODE_TEMPLATE.contains("{tv_mcp_root}"));
        assert!(TV_MCP_NODE_TEMPLATE.contains("{payloads_path}"));
    }

    #[test]
    fn embedded_template_has_kind_dispatch() {
        // Same idea — the template should dispatch on `item.kind` for
        // all four variants this crate emits.
        assert!(TV_MCP_NODE_TEMPLATE.contains("item.kind === 'pine_alertcondition'"));
        assert!(TV_MCP_NODE_TEMPLATE.contains("item.kind === 'vert_line_at'"));
        assert!(TV_MCP_NODE_TEMPLATE.contains("item.kind === 'price_value'"));
    }

    #[test]
    fn parse_node_output_extracts_results() {
        let stdout = br#"[{"name":"01-veto-too-high.yaml","status":200,"body":"{}"}]"#;
        let results = parse_node_output(Some(0), stdout, b"").expect("parse ok");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name.as_deref(), Some("01-veto-too-high.yaml"));
        assert_eq!(results[0].status, Some(200));
    }

    #[test]
    fn parse_node_output_propagates_error_field() {
        let stdout = br#"[{"name":"05-enter.yaml","error":"study not found"}]"#;
        let results = parse_node_output(Some(0), stdout, b"").expect("parse ok");
        assert_eq!(results[0].error.as_deref(), Some("study not found"));
        assert!(results[0].status.is_none());
    }

    #[test]
    fn parse_node_output_fails_on_non_zero_exit() {
        let res = parse_node_output(Some(1), b"", b"boom");
        let err = res.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("boom"), "msg = {msg}");
    }

    #[test]
    fn parse_node_output_fails_on_non_json_stdout() {
        let res = parse_node_output(Some(0), b"not json at all", b"");
        let err = res.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("non-JSON"), "msg = {msg}");
    }
}
