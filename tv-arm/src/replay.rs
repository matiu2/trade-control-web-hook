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

/// What this arm knew about itself, forwarded so a `--save`d fixture records
/// **which variant** it froze (`meta.json`'s `arm` block).
///
/// `replay-candles` can't derive any of this: the flags live here, and the plan
/// it receives doesn't carry them. `skip_calendar_bars` especially — a plan with
/// no pause rules could mean "calendar ran, no events" or "calendar skipped",
/// and only tv-arm knows which.
///
/// Forwarded only when the operator's passthrough contains `--save`; without a
/// save there's no fixture to annotate.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArmContext<'a> {
    pub skip_bcr: bool,
    pub strategy_v2: bool,
    pub skip_calendar_bars: bool,
    pub skip_golden: bool,
    /// `--start` exactly as the operator typed it.
    pub start: Option<&'a str>,
    /// The broker-qualified TradingView symbol the geometry came from.
    pub chart_symbol: Option<&'a str>,
}

impl ArmContext<'_> {
    /// The `--arm-*` tokens to append. Empty when the passthrough has no
    /// `--save`, since every one of those flags `requires = "save"` and would be
    /// a clap error otherwise.
    ///
    /// Each token is skipped when the operator already passed it, for the same
    /// reason the other defaults are: `ArgAction::Set` **rejects a repeated
    /// flag**, so an unconditional inject would make an explicit override a hard
    /// error rather than an override.
    fn argv(&self, passthrough: &[String]) -> Vec<String> {
        if !sets_flag(passthrough, "--save") {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut push_valued = |flag: &str, value: String| {
            if !sets_flag(passthrough, flag) {
                out.push(flag.to_string());
                out.push(value);
            }
        };
        push_valued(
            "--arm-entry-rule",
            match (self.skip_bcr, self.strategy_v2) {
                (true, false) => "skip-bcr",
                (false, true) => "strategy-v2",
                (true, true) => "skip-bcr+strategy-v2",
                (false, false) => "normal",
            }
            .to_string(),
        );
        if let Some(start) = self.start {
            push_valued("--arm-start", start.to_string());
        }
        if let Some(sym) = self.chart_symbol {
            push_valued("--arm-chart-symbol", sym.to_string());
        }
        push_valued("--arm-tv-arm-version", env!("GIT_VERSION").to_string());
        // Bare flags: only inject when set here AND absent from the passthrough
        // (`SetTrue` tolerates repeats, but stay consistent and quiet).
        if self.skip_calendar_bars && !sets_flag(passthrough, "--arm-skip-calendar-bars") {
            out.push("--arm-skip-calendar-bars".to_string());
        }
        if self.skip_golden && !sets_flag(passthrough, "--arm-skip-golden") {
            out.push("--arm-skip-golden".to_string());
        }
        out
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
/// operator's passthrough tokens. `argv[0]` is the binary name so the vector is
/// parseable by [`ReplayArgs::try_parse_from`] as-is.
///
/// **A default is only injected when the passthrough doesn't already set it.**
/// `ReplayArgs` declares these with `ArgAction::Set`, which **rejects a repeated
/// flag** ("cannot be used multiple times") instead of taking the last value — so
/// appending the passthrough after an unconditional default made the default
/// impossible to override. `--annotate false` on a chained replay was a hard
/// error with no escape (`--` doesn't help either), leaving no way to run an
/// unattended batch replay without drawing hundreds of positions onto the chart.
///
/// The prior version injected unconditionally, and its test asserted "last wins"
/// purely by *token position* without ever parsing the result — so the test
/// passed while every real invocation failed. Reported independently by both
/// `FEATURE-REQUEST-save-fixtures.md` and `DEV-BRIEF-postgres-candle-cache.md`.
fn build_argv(
    bin: &str,
    plan: &Path,
    source: CandleSource,
    passthrough: &[String],
    arm: ArmContext<'_>,
) -> Vec<String> {
    let mut argv = vec![
        bin.to_string(),
        "--plan".to_string(),
        plan.display().to_string(),
    ];
    if !sets_flag(passthrough, "--verbose") {
        argv.push("--verbose".to_string());
    }
    if !sets_flag(passthrough, "--annotate") {
        argv.push("--annotate".to_string());
        argv.push("true".to_string());
    }
    if !sets_flag(passthrough, "--source") {
        argv.push("--source".to_string());
        argv.push(source.as_str().to_string());
    }
    argv.extend(arm.argv(passthrough));
    argv.extend(passthrough.iter().cloned());
    argv
}

/// Does the passthrough already set `flag`? Matches both the separated form
/// (`--annotate false`) and the `=` form (`--annotate=false`).
fn sets_flag(passthrough: &[String], flag: &str) -> bool {
    let eq = format!("{flag}=");
    passthrough.iter().any(|a| a == flag || a.starts_with(&eq))
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
    arm: ArmContext<'_>,
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
    let argv = build_argv(&bin, &plan, source, passthrough, arm);

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
    fn argv_injects_defaults_when_passthrough_is_empty() {
        let plan = PathBuf::from("/tmp/p.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &[],
            ArmContext::default(),
        );
        assert_eq!(argv[0], "replay-candles");
        assert!(argv.contains(&"--verbose".to_string()));
        assert!(argv.contains(&"--annotate".to_string()));
        assert!(argv.contains(&"--source".to_string()));
        assert!(argv.contains(&"tradenation".to_string()));
        // And it must actually PARSE — the old test never checked this.
        let parsed = ReplayArgs::try_parse_from(&argv).expect("defaults must parse");
        assert!(parsed.annotate, "default is annotate on");
    }

    /// The regression this function exists for: an operator `--annotate false`
    /// must suppress our injected default and **parse**, not collide with it.
    ///
    /// The old test only asserted the passthrough token sat later in the vector
    /// and assumed clap's "last wins" — but `ArgAction::Set` rejects a repeated
    /// flag outright, so `--annotate true --annotate false` was a hard error. The
    /// test passed; the real command failed. Hence: parse, don't count positions.
    #[test]
    fn operator_annotate_false_overrides_the_default_and_parses() {
        let plan = PathBuf::from("/tmp/p.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--annotate".to_string(), "false".to_string()],
            ArmContext::default(),
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "--annotate").count(),
            1,
            "exactly one --annotate must survive: {argv:?}"
        );
        let parsed = ReplayArgs::try_parse_from(&argv)
            .expect("an operator --annotate false must not collide with the default");
        assert!(!parsed.annotate, "the operator's value must win");
    }

    /// The `=` form is the same override.
    #[test]
    fn operator_annotate_eq_form_also_overrides() {
        let plan = PathBuf::from("/tmp/p.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--annotate=false".to_string()],
            ArmContext::default(),
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("=-form must parse");
        assert!(!parsed.annotate);
    }

    /// Without `--save` there's no fixture to annotate, so no `--arm-*` tokens
    /// are emitted — they all `requires = "save"` and would be a clap error.
    #[test]
    fn arm_tokens_are_omitted_without_save() {
        let plan = PathBuf::from("/tmp/p.json");
        let arm = ArmContext {
            skip_bcr: true,
            skip_calendar_bars: true,
            ..Default::default()
        };
        let argv = build_argv("replay-candles", &plan, CandleSource::TradeNation, &[], arm);
        assert!(
            !argv.iter().any(|a| a.starts_with("--arm-")),
            "no --arm-* without --save: {argv:?}"
        );
        ReplayArgs::try_parse_from(&argv).expect("must parse");
    }

    /// With `--save`, the variant is recorded and the whole invocation parses.
    #[test]
    fn arm_tokens_record_the_variant_alongside_save() {
        let plan = PathBuf::from("/tmp/p.json");
        let arm = ArmContext {
            skip_bcr: true,
            skip_calendar_bars: true,
            skip_golden: true,
            start: Some("2026-07-17T17:00:00+10:00"),
            chart_symbol: Some("TRADENATION:EURUSD"),
            ..Default::default()
        };
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--save".to_string(), "trade-124".to_string()],
            arm,
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("arm tokens must parse with --save");
        assert_eq!(parsed.arm_entry_rule.as_deref(), Some("skip-bcr"));
        assert!(parsed.arm_skip_calendar_bars);
        assert!(parsed.arm_skip_golden);
        assert_eq!(
            parsed.arm_start.as_deref(),
            Some("2026-07-17T17:00:00+10:00")
        );
        assert_eq!(
            parsed.arm_chart_symbol.as_deref(),
            Some("TRADENATION:EURUSD")
        );
        assert!(
            parsed.arm_tv_arm_version.is_some(),
            "tv-arm stamps its own version"
        );
    }

    /// `strategy-v2` maps to its own column label.
    #[test]
    fn strategy_v2_records_its_own_entry_rule() {
        let plan = PathBuf::from("/tmp/p.json");
        let arm = ArmContext {
            strategy_v2: true,
            ..Default::default()
        };
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--save".to_string(), "t".to_string()],
            arm,
        );
        let parsed = ReplayArgs::try_parse_from(&argv).unwrap();
        assert_eq!(parsed.arm_entry_rule.as_deref(), Some("strategy-v2"));
    }

    /// An operator-supplied `--arm-*` must override ours, not collide with it —
    /// the same duplicate-flag trap as `--annotate`.
    #[test]
    fn operator_arm_entry_rule_overrides_without_colliding() {
        let plan = PathBuf::from("/tmp/p.json");
        let arm = ArmContext {
            skip_bcr: true,
            ..Default::default()
        };
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &[
                "--save".to_string(),
                "t".to_string(),
                "--arm-entry-rule".to_string(),
                "custom-thing".to_string(),
            ],
            arm,
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "--arm-entry-rule").count(),
            1,
            "exactly one must survive: {argv:?}"
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("override must parse");
        assert_eq!(parsed.arm_entry_rule.as_deref(), Some("custom-thing"));
    }

    /// Overriding `--source` and `--verbose` works the same way (they're injected
    /// defaults too, so they had the same latent collision).
    #[test]
    fn operator_source_override_does_not_collide() {
        let plan = PathBuf::from("/tmp/p.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--source".to_string(), "oanda".to_string()],
            ArmContext::default(),
        );
        assert_eq!(argv.iter().filter(|a| *a == "--source").count(), 1);
        let parsed = ReplayArgs::try_parse_from(&argv).expect("source override must parse");
        assert_eq!(parsed.source, CandleSource::Oanda);
    }

    #[test]
    fn argv_validates_against_shared_clap() {
        let plan = PathBuf::from("/tmp/p.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::Oanda,
            &[],
            ArmContext::default(),
        );
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
            ArmContext::default(),
        );
        assert!(
            ReplayArgs::try_parse_from(&argv).is_err(),
            "an unknown passthrough flag is caught by the shared clap parse"
        );
    }
}
