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

use std::path::{Path, PathBuf};
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

/// Explain a failure to launch a sibling CLI. The overwhelmingly common cause
/// is a **plain `cargo install --path journal`**, which bakes an EMPTY env
/// suffix, so this binary hunts for a bare `trade-control` / `tv-arm` — and the
/// unsuffixed CLIs were deliberately removed from this repo (only
/// `-dev` / `-staging` exist). A bare "No such file or directory" sends the
/// operator reading `cli.rs` to work that out, so say it here instead.
fn launch_error(program: &str, e: std::io::Error) -> color_eyre::Report {
    if ENV_SUFFIX.is_empty() && e.kind() == std::io::ErrorKind::NotFound {
        return eyre!(
            "failed to launch `{program}`: not found on PATH.\n\
             \n\
             This `journal` was built with NO environment suffix (a plain \
             `cargo build` / `cargo install --path journal`), so it looks for \
             the unsuffixed `{program}` — which this repo no longer installs.\n\
             \n\
             Run the deployed binary instead:\n    \
             journal-staging     (demo worker)\n    \
             journal-dev         (dev worker)\n\
             \n\
             Both are installed by ./deploy-staging.sh / ./deploy-dev.sh, which \
             bake the matching CLI suffix. To build a suffixed one by hand:\n    \
             TRADE_CONTROL_ENV_SUFFIX=staging cargo install --path journal"
        );
    }
    eyre!("failed to launch `{program}`: {e}")
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
    let out = cmd.output().map_err(|e| launch_error(&program, e))?;
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

/// Build the argv for re-blessing ONE saved fixture cell:
/// `--test-mode --fixture <CELL> --fixtures-dir <DIR> --rebless`.
///
/// Split out from [`rebless_fixture_cell`] so the argv is testable without
/// launching anything — every flag here is load-bearing and three of them are
/// silent-failure traps if dropped:
///
/// * **`--test-mode`** is what makes this read the frozen `plan.json` +
///   `candles.json` off disk instead of pulling from the broker. clap declares
///   it `requires = "fixture"`, so the pair moves together.
/// * **`--fixture <CELL>`** is the single-cell selector. The alternative,
///   `--fixtures-glob`, re-blesses the whole matching set — deliberately NOT
///   used here: a re-bless launched from one plan's page must not rewrite
///   another setup's goldens.
/// * **`--fixtures-dir <DIR>`** is passed ALWAYS, never left to the CLI's own
///   resolution. That default walks up from the cwd and falls back to a
///   build-time `CARGO_MANIFEST_DIR`, which in a deployed binary points at the
///   throwaway deploy worktree — the recorded trap where a `--rebless` covered
///   19 of 63 cells with no error either way. The caller passes the very
///   directory the cells were matched in ([`crate::fixtures::default_dir`]), so
///   what is re-blessed is what the info bar counted.
///
/// `--rebless` rewrites **only** `expected.json`; `meta.json`'s hand-written
/// `message`, the plan and the candles are untouched. That is exactly why a
/// re-bless is the right verb for "the behaviour changed on purpose" and a
/// re-capture is not.
fn rebless_args(cell: &str, fixtures_dir: &Path) -> Vec<String> {
    vec![
        "--test-mode".to_string(),
        "--fixture".to_string(),
        cell.to_string(),
        "--fixtures-dir".to_string(),
        fixtures_dir.to_string_lossy().to_string(),
        "--rebless".to_string(),
    ]
}

/// Re-bless one saved fixture cell — recompute its outcome from the frozen
/// plan + candles and overwrite its `expected.json`.
///
/// Offline: no broker, no chart, no TradingView. Returns the CLI's stdout
/// (ANSI stripped, as the replay paths do) so the caller can show the operator
/// what the new golden says.
pub fn rebless_fixture_cell(cell: &str, fixtures_dir: &Path) -> Result<String> {
    let program = bin("replay-candles");
    let args = rebless_args(cell, fixtures_dir);
    run_replay_candles(&program, &args)
}

/// Build the argv for a RAW replay — the stored plan, replayed as-is:
/// `--plan <FILE> --instrument <INST> --source <BROKER> --start <ARMED_AT>
/// --annotate true`.
///
/// "Raw" is the distinction from the journal's `r`: that one re-arms the setup
/// from the chart via tv-arm and replays what it just built, so it answers
/// "what would this setup do if armed today". This one feeds `replay-candles`
/// the plan the worker actually holds, so it answers "what does the plan that
/// is really out there do" — no chart read, no re-arm, no geometry drift.
///
/// **`--instrument` is not optional**, even though clap lets it be. Its
/// resolution order inside `replay-candles` is `--instrument` → **the live
/// TradingView chart symbol** → the plan, so the chart outranks the plan: omit
/// it and whatever pair the chart happens to be sitting on selects the candle
/// feed. That failure is silent — the plan's levels simply sit nowhere near the
/// prices, every entry is declined as outside the SL..TP range, and the run
/// reports a plausible 0R under a banner still naming the right instrument
/// (measured twice, most recently an AUD/NZD plan replayed at ~0.99 instead of
/// ~1.22, hiding a real −1.00R stop-out). Passing it also spares the run an MCP
/// round-trip to the chart it would otherwise need.
///
/// `--source` likewise comes from the plan's own broker rather than the CLI's
/// `tradenation` default, so an OANDA plan pulls OANDA candles.
///
/// **`--start` is not optional either**, for the same reason and with a louder
/// failure. `resolve_window` takes the window start from `--start` → **the
/// TradingView chart** → the plan, so omitting it hands the chart the window as
/// well as the feed. Measured against the real CLI on 2026-09-21 with an
/// expired plan (`hs-aud-nzd-ff8e66e8`): the chart supplied a start of
/// 2026-09-18 while the plan's expiry ended the window on 2026-09-09, and the
/// run died `bad-input: the replay window runs backwards: it ends 9.8 days
/// before it starts`. Passing the plan's own `armed_at` is what makes a raw
/// replay of an old plan work at all — and it is the same instant the journal's
/// re-armed replay already uses as its cursor, so the two runs cover the same
/// window and stay comparable.
///
/// `--annotate true` needs its explicit value (the flag is `ArgAction::Set`,
/// not a bare switch) and draws the simulated trades onto the chart, matching
/// what the tv-arm replay path already does.
fn raw_replay_args(
    plan_file: &Path,
    instrument: &str,
    broker: &str,
    armed_at: &str,
) -> Vec<String> {
    let mut args = vec![
        "--plan".to_string(),
        plan_file.to_string_lossy().to_string(),
        "--instrument".to_string(),
        instrument.to_string(),
        "--start".to_string(),
        armed_at.to_string(),
    ];
    if !broker.is_empty() {
        args.push("--source".to_string());
        args.push(broker.to_string());
    }
    args.push("--annotate".to_string());
    args.push("true".to_string());
    args
}

/// Replay the STORED plan as-is: export it from the worker to a temp file, then
/// `replay-candles --plan <FILE> --instrument <INST> --source <BROKER>`.
///
/// See [`raw_replay_args`] for why each flag is there. The plan file is written
/// under the OS temp dir keyed by `trade_id`, mirroring where tv-arm puts its
/// own replay plan, and is left in place afterwards so an operator can re-run
/// the same command by hand from the status line.
pub fn raw_replay(
    trade_id: &str,
    instrument: &str,
    broker: &str,
    armed_at: &str,
) -> Result<String> {
    let plan_json = plan_export_json(trade_id)?;
    let plan_file = std::env::temp_dir().join(format!("journal-raw-replay-{trade_id}.json"));
    std::fs::write(&plan_file, plan_json)
        .map_err(|e| eyre!("write plan to {}: {e}", plan_file.display()))?;
    let program = bin("replay-candles");
    let args = raw_replay_args(&plan_file, instrument, broker, armed_at);
    run_replay_candles(&program, &args)
}

/// Run `replay-candles-<env>` with `args`, returning ANSI-stripped stdout.
/// Shared by the re-bless and raw-replay paths, which differ only in argv —
/// the quieting, the ANSI strip and the failure shape are identical to the
/// tv-arm wrappers above and must not drift from them.
fn run_replay_candles(program: &str, args: &[String]) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if std::env::var_os("RUST_LOG").is_none() {
        cmd.env("RUST_LOG", "warn");
    }
    let out = cmd.output().map_err(|e| launch_error(program, e))?;
    let stdout = strip_ansi(&String::from_utf8_lossy(&out.stdout));
    if !out.status.success() {
        let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
        return Err(eyre!(
            "`{program} {}` failed ({}): {}\n{stdout}",
            args.join(" "),
            out.status,
            stderr.trim()
        ));
    }
    Ok(stdout)
}

/// Build the argv for a replay: `tv-arm [--spec-url <URL>] --start <armed_at>
/// [skip flags] replay`.
///
/// Split out from [`replay_via_tv_arm`] so the flags — and critically the
/// PRESENCE of `--spec-url` — are testable without launching anything, the same
/// way [`save_fixture_args`] already is. Order is load-bearing for the same
/// reason: every one of these is a **tv-arm** flag, so all must precede the
/// `replay` subcommand or clap rejects them.
fn replay_args<'a>(
    armed_at: &'a str,
    skip_flags: &[&'a str],
    spec_url: Option<&'a str>,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(url) = spec_url {
        args.push("--spec-url".to_string());
        args.push(url.to_string());
    }
    args.push("--start".to_string());
    args.push(armed_at.to_string());
    args.extend(skip_flags.iter().map(|s| s.to_string()));
    args.push("replay".to_string());
    args
}

/// Replay the setup by re-arming it from the active chart backend and chaining
/// into `replay-candles`, via `tv-arm-<env> [--spec-url …] --start <armed_at>
/// replay`.
///
/// `spec_url` selects WHICH chart is re-armed, and is the whole point of the
/// parameter (see [`crate::tv::ChartBackend::spec_url`] for the incident that
/// motivated it):
///
/// * `None` (TradingView, the default) — tv-arm reads the **live chart** it has
///   already loaded, taking the instrument, timeframe, and **broker from the
///   chart's own exchange**. No `--instrument`/`--source` to pass, and no
///   instrument-resolution failure for OANDA-only assets (e.g. the XAU/XAG
///   ratio that isn't listed on TradeNation). Byte-identical to before this
///   parameter existed.
/// * `Some(url)` (`--new-tv`) — tv-arm fetches a frozen setup from
///   local-chart's `GET /arm-setup` and never touches TradingView at all.
///   Without this the two backends fall out of step: `l` navigates
///   local-chart while the replay silently arms off a stale TradingView tab.
///
/// `--start <armed_at>` is the "live now" cursor in both cases: on the chart
/// path tv-arm walks the chart to find the pattern's roles (neckline /
/// invalidation / expiry) relative to it, and on the spec path it **overrides
/// the frozen cursor** (`tv-arm/src/pipeline.rs`: `parse_start(args)?.or(
/// frozen.start)`), so the journal's `armed_at` still means exactly what it
/// meant before.
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
/// `skip_flags` are tv-arm's own prep-skip flags (`--skip-break-and-close`,
/// `--skip-retest`) that must match how the ORIGINAL plan was armed. tv-arm
/// re-arms from the chart and defaults to the FULL break-and-close-then-retest,
/// so a plan armed with `--skip-bcr` would otherwise re-arm WITH the preps and
/// stall in `AwaitBreakAndClose` — a replay↔original divergence. The journal
/// reads the stored plan's preps and forwards the matching skips. These are
/// tv-arm flags, so they go **before** the `replay` subcommand.
pub fn replay_via_tv_arm(
    armed_at: &str,
    skip_flags: &[&str],
    spec_url: Option<&str>,
) -> Result<String> {
    let program = bin("tv-arm");
    let args = replay_args(armed_at, skip_flags, spec_url);
    let mut cmd = Command::new(&program);
    cmd.args(&args);
    // tv-arm logs its pipeline at INFO on **stdout** (mixed into the report we
    // capture); quiet it to warn so the report body dominates. Honour an
    // operator's own RUST_LOG if they set one. (ANSI is stripped regardless.)
    if std::env::var_os("RUST_LOG").is_none() {
        cmd.env("RUST_LOG", "warn");
    }
    let out = cmd.output().map_err(|e| launch_error(&program, e))?;
    let stdout = strip_ansi(&String::from_utf8_lossy(&out.stdout));
    if !out.status.success() {
        let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
        return Err(eyre!(
            "`{program} {}` failed ({}): {}\n{stdout}",
            args.join(" "),
            out.status,
            stderr.trim()
        ));
    }
    Ok(stdout)
}

/// Build the argv for a fixture capture: `tv-arm --start <armed_at>
/// [skip flags] --save-fixture --fixture-name <id> [--message <text>] replay`.
///
/// Split out from [`save_fixture_via_tv_arm`] so the flag ORDER is testable
/// without launching anything. Order is load-bearing: `--save-fixture` and its
/// `--fixture-name` / `--message` are **tv-arm** flags, so they must precede the
/// `replay` subcommand — clap rejects them after it. Same rule as the skip flags.
fn save_fixture_args<'a>(
    armed_at: &'a str,
    skip_flags: &[&'a str],
    fixture_name: &'a str,
    message: Option<&'a str>,
    spec_url: Option<&'a str>,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(url) = spec_url {
        args.push("--spec-url".to_string());
        args.push(url.to_string());
    }
    args.push("--start".to_string());
    args.push(armed_at.to_string());
    args.extend(skip_flags.iter().map(|s| s.to_string()));
    args.push("--save-fixture".to_string());
    args.push("--fixture-name".to_string());
    args.push(fixture_name.to_string());
    if let Some(m) = message {
        args.push("--message".to_string());
        args.push(m.to_string());
    }
    args.push("replay".to_string());
    args
}

/// Capture the six-cell fixture corpus for this setup by re-arming it from the
/// active chart backend, via `tv-arm --save-fixture … replay`.
///
/// `--save-fixture` is tv-arm's one-flag corpus capture: it freezes the setup to
/// a `.spec.json`, arms all six grid cells (normal / skip-bcr / strategy-v2
/// × news on/off), and saves each cell's candles + expected outcome under
/// `replay-fixtures/`. So this shares the replay's hard precondition — the
/// plan's chart must already be loaded, with its drawings intact — because
/// tv-arm reads whatever chart is up.
///
/// `spec_url` carries the replay's meaning verbatim (`None` = live TradingView,
/// `Some` = local-chart's `/arm-setup`), and matters MORE here: a replay off
/// the wrong chart is a wrong answer the operator reads once, but a fixture off
/// the wrong chart is a wrong expectation **committed to the corpus**, where it
/// then pins the wrong gates for every future run.
///
/// `fixture_name` is passed explicitly (the journal uses the plan's `trade_id`)
/// so a captured fixture traces back to the journal page it came from, rather
/// than tv-arm's derived `<instrument>-<granularity>-<date>` name which would
/// collide across two setups armed on the same instrument the same day.
///
/// The `skip_flags` caveat is the replay's, verbatim: a plan armed with
/// `--skip-bcr` must re-arm with the skips or the captured fixtures pin the
/// WRONG gate set. Returns the capture's stdout (ANSI stripped, as the replay).
pub fn save_fixture_via_tv_arm(
    armed_at: &str,
    skip_flags: &[&str],
    fixture_name: &str,
    message: Option<&str>,
    spec_url: Option<&str>,
) -> Result<String> {
    let program = bin("tv-arm");
    let args = save_fixture_args(armed_at, skip_flags, fixture_name, message, spec_url);
    let mut cmd = Command::new(&program);
    cmd.args(&args);
    if std::env::var_os("RUST_LOG").is_none() {
        cmd.env("RUST_LOG", "warn");
    }
    let out = cmd.output().map_err(|e| launch_error(&program, e))?;
    let stdout = strip_ansi(&String::from_utf8_lossy(&out.stdout));
    if !out.status.success() {
        let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
        return Err(eyre!(
            "`{program} {}` failed ({}): {}\n{stdout}",
            args.join(" "),
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

    /// A missing sibling CLI in an UNSUFFIXED build is the `cargo install
    /// --path journal` mistake — the error must name the fix, not just report
    /// ENOENT and leave the operator reading this file.
    #[test]
    fn launch_error_explains_the_unsuffixed_build() {
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        let msg = launch_error("trade-control", e).to_string();
        if ENV_SUFFIX.is_empty() {
            assert!(msg.contains("journal-staging"), "names the fix:\n{msg}");
            assert!(
                msg.contains("TRADE_CONTROL_ENV_SUFFIX"),
                "names the build-it-yourself route:\n{msg}"
            );
        } else {
            // A suffixed build has a real missing-binary problem; stay terse.
            assert!(msg.contains("failed to launch"), "{msg}");
        }
    }

    /// Any OTHER launch failure (a permission error, say) is a genuine problem
    /// and must not be misreported as the suffix mistake.
    #[test]
    fn launch_error_does_not_blame_the_suffix_for_other_errors() {
        let e = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let msg = launch_error("trade-control", e).to_string();
        assert!(!msg.contains("journal-staging"), "{msg}");
    }

    /// `--save-fixture` and its companions are **tv-arm** flags, so they must
    /// come BEFORE the `replay` subcommand — clap rejects them after it. This is
    /// the same ordering trap the skip flags have.
    #[test]
    fn save_fixture_flags_precede_the_replay_subcommand() {
        let args = save_fixture_args("2026-07-22T20:58:53Z", &[], "trade-1", None, None);
        let replay_at = args.iter().position(|a| a == "replay");
        let save_at = args.iter().position(|a| a == "--save-fixture");
        let name_at = args.iter().position(|a| a == "--fixture-name");
        assert!(replay_at.is_some(), "the subcommand is present: {args:?}");
        assert!(
            save_at < replay_at,
            "--save-fixture before replay: {args:?}"
        );
        assert!(
            name_at < replay_at,
            "--fixture-name before replay: {args:?}"
        );
        // `replay` is last, so anything appended later stays a replay-side flag.
        assert_eq!(args.last().map(String::as_str), Some("replay"), "{args:?}");
    }

    /// The fixture name is the plan's trade_id, so a capture traces back to the
    /// journal page rather than colliding on tv-arm's derived date-based name.
    #[test]
    fn save_fixture_names_the_fixture_after_the_trade() {
        let args = save_fixture_args(
            "2026-07-22T20:58:53Z",
            &[],
            "ihs-eur-usd-584d3770",
            None,
            None,
        );
        let i = args
            .iter()
            .position(|a| a == "--fixture-name")
            .unwrap_or_default();
        assert_eq!(
            args.get(i + 1).map(String::as_str),
            Some("ihs-eur-usd-584d3770")
        );
    }

    /// A skip-BCR plan must capture WITH its skip flags, or the fixtures pin the
    /// wrong gate set — the same divergence the replay path guards against.
    #[test]
    fn save_fixture_forwards_the_skip_flags_before_replay() {
        let args = save_fixture_args(
            "2026-07-22T20:58:53Z",
            &["--skip-break-and-close", "--skip-retest"],
            "trade-1",
            None,
            None,
        );
        let replay_at = args.iter().position(|a| a == "replay");
        for flag in ["--skip-break-and-close", "--skip-retest"] {
            let at = args.iter().position(|a| a == flag);
            assert!(at.is_some(), "{flag} forwarded: {args:?}");
            assert!(at < replay_at, "{flag} before replay: {args:?}");
        }
    }

    /// `--message` is optional: absent means no flag at all (not an empty one,
    /// which clap would reject as a missing value).
    #[test]
    fn save_fixture_omits_message_when_none() {
        let without = save_fixture_args("2026-07-22T20:58:53Z", &[], "t", None, None);
        assert!(!without.iter().any(|a| a == "--message"), "{without:?}");
        let with = save_fixture_args(
            "2026-07-22T20:58:53Z",
            &[],
            "t",
            Some("why it exists"),
            None,
        );
        let i = with
            .iter()
            .position(|a| a == "--message")
            .unwrap_or_default();
        assert_eq!(with.get(i + 1).map(String::as_str), Some("why it exists"));
    }

    /// Default (TradingView): NO `--spec-url`, so tv-arm reads the live chart.
    /// Byte-identical to the argv this built before the parameter existed —
    /// the whole point of `None` being the TradingView answer.
    #[test]
    fn replay_without_a_spec_url_is_the_original_argv() {
        let args = replay_args("2026-07-22T20:58:53Z", &["--skip-retest"], None);
        assert_eq!(
            args,
            vec!["--start", "2026-07-22T20:58:53Z", "--skip-retest", "replay"]
        );
    }

    /// `--new-tv`: the replay must arm from local-chart's `/arm-setup`, not the
    /// live TradingView chart. Without this the two chart seams diverge — `l`
    /// navigates local-chart while the replay silently arms off whatever
    /// TradingView is showing (the `ihs-eur-cad` incident: an invalidation line
    /// at 1.61982 against a plan whose `too-low` was 1.60942).
    #[test]
    fn replay_passes_the_spec_url_before_the_replay_subcommand() {
        let url = "http://127.0.0.1:8790/arm-setup?instrument=EUR_CAD&tf=h1";
        let args = replay_args("2026-07-22T20:58:53Z", &[], Some(url));
        let at = args.iter().position(|a| a == "--spec-url");
        assert!(at.is_some(), "--spec-url forwarded: {args:?}");
        let at = at.unwrap_or_default();
        assert_eq!(args.get(at + 1).map(String::as_str), Some(url));
        // A tv-arm flag, so it must precede the subcommand or clap rejects it.
        assert!(
            Some(at) < args.iter().position(|a| a == "replay"),
            "--spec-url before replay: {args:?}"
        );
        // `--start` still overrides the frozen cursor, so armed_at keeps meaning.
        assert!(args.iter().any(|a| a == "--start"), "{args:?}");
    }

    /// The skip flags still reach a spec-url arm. `--spec-url` changes only
    /// WHERE the geometry comes from; the prep set still has to reproduce the
    /// original plan's or the replay diverges for the other reason.
    #[test]
    fn replay_keeps_the_skip_flags_alongside_a_spec_url() {
        let args = replay_args(
            "2026-07-22T20:58:53Z",
            &["--skip-break-and-close", "--skip-retest"],
            Some("http://127.0.0.1:8790/arm-setup?instrument=EUR_CAD&tf=h1"),
        );
        let replay_at = args.iter().position(|a| a == "replay");
        for flag in ["--skip-break-and-close", "--skip-retest"] {
            let at = args.iter().position(|a| a == flag);
            assert!(at.is_some(), "{flag} forwarded: {args:?}");
            assert!(at < replay_at, "{flag} before replay: {args:?}");
        }
    }

    /// The capture path has the SAME defect and the same fix — and higher
    /// stakes, since a fixture armed off the wrong chart is committed to the
    /// corpus as a wrong expectation.
    #[test]
    fn save_fixture_passes_the_spec_url_before_the_replay_subcommand() {
        let url = "http://127.0.0.1:8790/arm-setup?instrument=EUR_CAD&tf=h1";
        let args = save_fixture_args("2026-07-22T20:58:53Z", &[], "trade-1", None, Some(url));
        let at = args.iter().position(|a| a == "--spec-url");
        assert!(at.is_some(), "--spec-url forwarded: {args:?}");
        assert_eq!(
            args.get(at.unwrap_or_default() + 1).map(String::as_str),
            Some(url)
        );
        assert!(at < args.iter().position(|a| a == "replay"), "{args:?}");
        // The capture's own flags are untouched by the addition.
        assert!(args.iter().any(|a| a == "--save-fixture"), "{args:?}");
        assert_eq!(args.last().map(String::as_str), Some("replay"), "{args:?}");
    }

    /// Absent spec-url must emit NO flag at all — not an empty one, which clap
    /// would reject as a missing value (the same trap `--message` documents).
    #[test]
    fn no_spec_url_emits_no_flag_on_either_path() {
        let replay = replay_args("2026-07-22T20:58:53Z", &[], None);
        assert!(!replay.iter().any(|a| a == "--spec-url"), "{replay:?}");
        let fixture = save_fixture_args("2026-07-22T20:58:53Z", &[], "t", None, None);
        assert!(!fixture.iter().any(|a| a == "--spec-url"), "{fixture:?}");
    }

    /// `--test-mode` and `--fixture` move together (clap declares
    /// `requires = "fixture"`), and the cell named is the ONE cell re-blessed.
    #[test]
    fn rebless_selects_exactly_one_cell() {
        let args = rebless_args(
            "aud-nzd-h1-2026-09-04-normal-news-off",
            Path::new("/repo/replay-fixtures"),
        );
        assert!(args.iter().any(|a| a == "--test-mode"), "{args:?}");
        assert!(args.iter().any(|a| a == "--rebless"), "{args:?}");
        let i = args
            .iter()
            .position(|a| a == "--fixture")
            .unwrap_or_default();
        assert_eq!(
            args.get(i + 1).map(String::as_str),
            Some("aud-nzd-h1-2026-09-04-normal-news-off")
        );
        // The whole-corpus form must NOT appear: a re-bless launched from one
        // plan's page must never rewrite another setup's goldens.
        assert!(!args.iter().any(|a| a == "--fixtures-glob"), "{args:?}");
    }

    /// `--fixtures-dir` is ALWAYS passed, never left to the CLI's own
    /// resolution — that default walks up from the cwd and falls back to a
    /// build-time manifest path, which in a deployed binary is a deleted deploy
    /// worktree. The recorded cost: a `--rebless` that covered 19 of 63 cells
    /// with no error either way.
    #[test]
    fn rebless_always_passes_the_fixtures_dir() {
        let args = rebless_args("cell", Path::new("/some/where/replay-fixtures"));
        let i = args
            .iter()
            .position(|a| a == "--fixtures-dir")
            .unwrap_or_else(|| panic!("--fixtures-dir must be explicit: {args:?}"));
        assert_eq!(
            args.get(i + 1).map(String::as_str),
            Some("/some/where/replay-fixtures")
        );
    }

    /// `--rebless` refuses at the CLI under `--simulate false`, `--cron-gap N`
    /// and `--upkeep`, so this argv must carry none of them — otherwise the
    /// re-bless fails as bad input instead of writing the new golden.
    #[test]
    fn rebless_carries_nothing_that_would_make_the_cli_refuse() {
        let args = rebless_args("cell", Path::new("/repo/replay-fixtures"));
        for forbidden in ["--simulate", "--cron-gap", "--upkeep", "--check"] {
            assert!(
                !args.iter().any(|a| a == forbidden),
                "{forbidden} makes --rebless refuse: {args:?}"
            );
        }
    }

    /// An arm time to build raw-replay argv against.
    const TS: &str = "2026-09-04T09:50:43Z";

    /// The OTHER raw-replay trap, and the one that fails loudly rather than
    /// quietly: without `--start`, `replay-candles` takes the window start from
    /// the TradingView chart. Measured against the real CLI with an expired
    /// plan — the chart said 2026-09-18, the plan's expiry ended the window on
    /// 2026-09-09, and the run died `bad-input: the replay window runs
    /// backwards`. So a raw replay of any plan whose chart has moved on is
    /// simply impossible without this flag.
    #[test]
    fn raw_replay_always_names_the_start() {
        let args = raw_replay_args(Path::new("/tmp/p.json"), "AUD/NZD", "tradenation", TS);
        let i = args
            .iter()
            .position(|a| a == "--start")
            .unwrap_or_else(|| panic!("--start must be explicit: {args:?}"));
        assert_eq!(args.get(i + 1).map(String::as_str), Some(TS));
    }

    /// **The** raw-replay trap: without `--instrument`, `replay-candles` ranks
    /// the live TradingView chart's symbol ABOVE the plan's own instrument, so
    /// whatever pair the chart sits on picks the candle feed. It fails
    /// silently — a plausible 0R under a banner naming the right instrument.
    #[test]
    fn raw_replay_always_names_the_instrument() {
        let args = raw_replay_args(Path::new("/tmp/p.json"), "AUD/NZD", "tradenation", TS);
        let i = args
            .iter()
            .position(|a| a == "--instrument")
            .unwrap_or_else(|| panic!("--instrument must be explicit: {args:?}"));
        assert_eq!(args.get(i + 1).map(String::as_str), Some("AUD/NZD"));
    }

    /// The plan file is the stored plan, and the source is the plan's OWN
    /// broker — not the CLI's `tradenation` default, which would pull the wrong
    /// feed for an OANDA plan.
    #[test]
    fn raw_replay_passes_the_plan_file_and_the_plans_broker() {
        let args = raw_replay_args(
            Path::new("/tmp/journal-raw-replay-x.json"),
            "EUR_CAD",
            "oanda",
            TS,
        );
        let p = args.iter().position(|a| a == "--plan").unwrap_or_default();
        assert_eq!(
            args.get(p + 1).map(String::as_str),
            Some("/tmp/journal-raw-replay-x.json")
        );
        let s = args
            .iter()
            .position(|a| a == "--source")
            .unwrap_or_default();
        assert_eq!(args.get(s + 1).map(String::as_str), Some("oanda"));
    }

    /// An unknown broker (no rule carried one) must emit NO `--source` at all,
    /// not an empty value — clap would reject the empty string as an invalid
    /// variant and the replay would never start. Same trap `--message` and
    /// `--spec-url` document on the tv-arm paths.
    #[test]
    fn raw_replay_omits_an_unknown_source() {
        let args = raw_replay_args(Path::new("/tmp/p.json"), "AUD/NZD", "", TS);
        assert!(!args.iter().any(|a| a == "--source"), "{args:?}");
        // The instrument is still named — it is the flag that must never drop.
        assert!(args.iter().any(|a| a == "--instrument"), "{args:?}");
    }

    /// `--annotate` is `ArgAction::Set`, not a bare switch: the value must be
    /// spelled out or clap consumes the next token as its value.
    #[test]
    fn raw_replay_spells_out_the_annotate_value() {
        let args = raw_replay_args(Path::new("/tmp/p.json"), "AUD/NZD", "tradenation", TS);
        let i = args
            .iter()
            .position(|a| a == "--annotate")
            .unwrap_or_default();
        assert_eq!(args.get(i + 1).map(String::as_str), Some("true"));
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
