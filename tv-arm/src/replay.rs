//! `tv-arm ... replay`: chain straight into `replay-candles` on the plan we
//! just built (the `replay` subcommand; it builds the plan but does NOT arm it).
//!
//! The plan JSON is already on disk (written by `register_trade_plan` to a temp
//! path we synthesise for the `replay` subcommand). This module assembles the
//! `replay-candles` invocation — sensible defaults
//! (`--verbose --annotate true --source <broker>`) plus any passthrough tokens
//! the operator put after `replay`, which override the defaults — validates
//! it against the SHARED [`ReplayArgs`] clap definition, then shells out to the
//! environment-matched `replay-candles-<suffix>` binary.
//!
//! Sharing `ReplayArgs` (from `trade-control-cli`) is what keeps this honest:
//! the same struct the standalone binary parses is what we validate against
//! here, so a passthrough flag that `replay-candles` wouldn't accept fails
//! before we shell out, with `replay-candles`' own error text.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Parser as _;
use color_eyre::eyre::{Result, eyre};
use tracing::{info, warn};
use trade_control_cli::replay_args::{CandleSource, ReplayArgs};
use trade_control_conventions::Broker;

/// Environment suffix baked at build time (`dev` / `staging`); empty for a
/// plain `cargo build`. Selects the `replay-candles-<suffix>` binary so
/// `tv-arm-staging --replay` runs `replay-candles-staging`.
const BAKED_ENV_SUFFIX: &str = env!("BAKED_ENV_SUFFIX");

/// The `replay-candles` binary name for this environment. `replay-candles-dev`
/// / `replay-candles-staging` when a suffix is baked, else the plain
/// `replay-candles` on `PATH` (a no-suffix `cargo install`).
fn replay_binary() -> String {
    if BAKED_ENV_SUFFIX.is_empty() {
        "replay-candles".to_string()
    } else {
        format!("replay-candles-{BAKED_ENV_SUFFIX}")
    }
}

/// Map the resolved broker to the `--source` value `replay-candles` expects.
/// The live cron engine pulls TradeNation candles, so a TradeNation-armed plan
/// replays against TradeNation; an OANDA plan against OANDA.
fn source_for(broker: Broker) -> CandleSource {
    match broker {
        Broker::TradeNation => CandleSource::TradeNation,
        Broker::Oanda => CandleSource::Oanda,
    }
}

/// Resolve the plan path to replay against. When an explicit destination is
/// given, replay that JSON; otherwise a temp path derived from the trade id,
/// which `register_trade_plan` also wrote to. The `replay` subcommand always
/// passes `None` here (it never names a file), so it replays the temp path.
pub fn plan_path(plan_out: Option<&Path>, trade_id: &str) -> PathBuf {
    match plan_out {
        Some(p) => p.to_path_buf(),
        None => std::env::temp_dir().join(format!("tv-arm-replay-{trade_id}.json")),
    }
}

/// Build the `replay-candles` argument vector: our defaults first, then the
/// operator's passthrough tokens (which override, since clap's last value
/// wins). `argv[0]` is the binary name so the vector is parseable by
/// [`ReplayArgs::try_parse_from`] as-is.
fn build_argv(bin: &str, plan: &Path, source: CandleSource, passthrough: &[String]) -> Vec<String> {
    let mut argv = vec![
        bin.to_string(),
        "--plan".to_string(),
        plan.display().to_string(),
        "--verbose".to_string(),
        "--annotate".to_string(),
        "true".to_string(),
        "--source".to_string(),
        source.as_str().to_string(),
    ];
    argv.extend(passthrough.iter().cloned());
    argv
}

/// Validate + run `replay-candles` on the freshly-built plan. Stdout/stderr are
/// inherited so the replay report streams straight to the operator's terminal.
///
/// A non-zero exit from `replay-candles` is surfaced as an error (so a failed
/// replay is visible), but the plan itself is already armed by the time we get
/// here — the replay is a post-arm convenience, not part of arming.
pub fn run_replay(
    plan_out: Option<&Path>,
    trade_id: &str,
    broker: Broker,
    passthrough: &[String],
) -> Result<()> {
    let bin = replay_binary();
    let plan = plan_path(plan_out, trade_id);
    if !plan.exists() {
        return Err(eyre!(
            "--replay: plan JSON not found at {} (expected it to be written before replay)",
            plan.display()
        ));
    }
    let source = source_for(broker);
    let argv = build_argv(&bin, &plan, source, passthrough);

    // Validate the full invocation against the shared clap definition before
    // shelling out, so a bad passthrough flag fails with replay-candles' own
    // error rather than an opaque non-zero exit. (We discard the parsed value —
    // the actual run is the subprocess, which reparses identically.)
    ReplayArgs::try_parse_from(&argv)
        .map_err(|e| eyre!("--replay: invalid replay-candles arguments: {e}"))?;

    info!(
        binary = %bin,
        plan = %plan.display(),
        source = source.as_str(),
        passthrough = passthrough.len(),
        "chaining into replay-candles (--replay)"
    );

    // argv[0] is the binary name for the clap validate above; skip it here.
    let status = Command::new(&bin)
        .args(&argv[1..])
        .status()
        .map_err(|e| eyre!("--replay: failed to launch {bin}: {e}"))?;

    if !status.success() {
        warn!(binary = %bin, code = ?status.code(), "replay-candles exited non-zero");
        return Err(eyre!(
            "--replay: {bin} exited with status {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string())
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_name_respects_suffix() {
        // The resolved name is exactly what the baked suffix dictates: empty
        // suffix → the plain `replay-candles`; a `staging`/`dev` bake →
        // `replay-candles-<suffix>`. This keys off BAKED_ENV_SUFFIX so a
        // `TRADE_CONTROL_ENV_SUFFIX=staging cargo test` proves the staging path.
        let name = replay_binary();
        if BAKED_ENV_SUFFIX.is_empty() {
            assert_eq!(name, "replay-candles");
        } else {
            assert_eq!(name, format!("replay-candles-{BAKED_ENV_SUFFIX}"));
        }
    }

    #[test]
    fn source_maps_broker() {
        assert_eq!(source_for(Broker::TradeNation).as_str(), "tradenation");
        assert_eq!(source_for(Broker::Oanda).as_str(), "oanda");
    }

    #[test]
    fn plan_path_prefers_plan_out() {
        let out = PathBuf::from("/tmp/my-plan.json");
        assert_eq!(plan_path(Some(&out), "T123"), out);
    }

    #[test]
    fn plan_path_falls_back_to_temp_with_trade_id() {
        let p = plan_path(None, "T123");
        assert!(p.to_string_lossy().contains("tv-arm-replay-T123.json"));
    }

    #[test]
    fn argv_has_defaults_then_passthrough() {
        let plan = PathBuf::from("/tmp/p.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--annotate".to_string(), "false".to_string()],
        );
        // defaults present
        assert_eq!(argv[0], "replay-candles");
        assert!(argv.contains(&"--verbose".to_string()));
        assert!(argv.contains(&"--source".to_string()));
        assert!(argv.contains(&"tradenation".to_string()));
        // passthrough appended AFTER the defaults so it overrides (clap: last wins)
        let first_annotate = argv.iter().position(|a| a == "--annotate").unwrap();
        let last_annotate = argv.iter().rposition(|a| a == "--annotate").unwrap();
        assert!(
            last_annotate > first_annotate,
            "passthrough --annotate comes last"
        );
    }

    #[test]
    fn argv_validates_against_shared_clap() {
        let plan = PathBuf::from("/tmp/p.json");
        let argv = build_argv("replay-candles", &plan, CandleSource::Oanda, &[]);
        assert!(
            ReplayArgs::try_parse_from(&argv).is_ok(),
            "default argv parses against ReplayArgs"
        );
    }

    #[test]
    fn argv_rejects_unknown_passthrough_flag() {
        let plan = PathBuf::from("/tmp/p.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::Oanda,
            &["--no-such-flag".to_string()],
        );
        assert!(
            ReplayArgs::try_parse_from(&argv).is_err(),
            "an unknown passthrough flag is caught by the shared clap parse"
        );
    }
}
