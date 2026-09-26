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
//! Under `--new-tv` the `--annotate true` default is dropped: local-chart is
//! the chart, and `replay-candles` is asked for a `--positions` file instead.
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
    /// `--qm-entry` — which order type the strategy-v2 QM leg uses. `None` is the
    /// default (limit), which the plain `strategy-v2` label already covers.
    ///
    /// Recorded because the QM leg's order type is a **separate axis** from the
    /// entry rule: a market QM answers "is the confirmation candle alone enough?"
    /// while the default limit asks "does waiting for the pullback pay for the
    /// fills it misses?". Collapsing them into one label would average a
    /// fill-rate difference into a returns difference.
    pub qm_entry: Option<crate::args::QmEntry>,
    pub skip_calendar_bars: bool,
    pub skip_golden: bool,
    /// `--skip-reversals` — both reversal-closes dropped. The grid's exit axis,
    /// recorded for the same reason as `skip_calendar_bars`: the replay can't
    /// tell "reversals skipped" from "nothing drawn to reverse off".
    pub skip_reversals: bool,
    /// `--start` exactly as the operator typed it.
    pub start: Option<&'a str>,
    /// The broker-qualified TradingView symbol the geometry came from.
    pub chart_symbol: Option<&'a str>,
    /// The **broker** instrument symbol the plan was built for (`AUD/NZD`,
    /// `EUR_USD`) — the resolved form, not the qualified chart symbol.
    ///
    /// Forwarded as `--instrument` so the replay pulls the candles this plan's
    /// levels were drawn against. Without it `replay-candles` resolves
    /// `--instrument` → **TradingView chart** → plan (`resolve_window`), and the
    /// chart sits *ahead* of the plan: a chart left on another pair replays the
    /// plan against a different instrument's prices, producing a plausible
    /// report full of declined entries. See
    /// `argv_forwards_the_plans_instrument`.
    ///
    /// Unlike the `--arm-*` context this is **not** gated on `--save`: it
    /// selects the candle feed, so every chained replay needs it.
    pub instrument: Option<&'a str>,
}

impl ArmContext<'_> {
    /// The grid-column label for this arm — the `--arm-entry-rule` value.
    ///
    /// **Must** match `EntryRule::parse`/`label` on the replay side
    /// (`replay_candles::arm_record`), which is what a batch tool groups columns
    /// on. A label that doesn't parse there degrades to `EntryRule::Other`,
    /// which is recorded honestly but sits outside the known grid.
    ///
    /// The QM entry mode only qualifies the label when `strategy_v2` is on:
    /// `--qm-entry` `requires = "strategy_v2"` at the clap layer, so the
    /// combination can't otherwise occur, and reading it unconditionally would
    /// invent labels for arms that never had a QM leg.
    pub(crate) fn entry_rule_label(&self) -> String {
        match (self.skip_bcr, self.strategy_v2) {
            (true, false) => "skip-bcr".to_string(),
            (false, true) => match self.qm_entry {
                // The default QM leg is a limit, which is what plain
                // `strategy-v2` has always meant — keep that label byte-identical
                // so fixtures captured before `--qm-entry` existed still group
                // into the same column.
                None | Some(crate::args::QmEntry::Limit) => "strategy-v2".to_string(),
                Some(crate::args::QmEntry::Market) => "strategy-v2-qm-market".to_string(),
                Some(crate::args::QmEntry::Stop) => "strategy-v2-qm-stop".to_string(),
            },
            (true, true) => "skip-bcr+strategy-v2".to_string(),
            (false, false) => "normal".to_string(),
        }
    }

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
        push_valued("--arm-entry-rule", self.entry_rule_label());
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
        if self.skip_reversals && !sets_flag(passthrough, "--arm-skip-reversals") {
            out.push("--arm-skip-reversals".to_string());
        }
        out
    }
}

/// Map the resolved broker to the `--source` value `replay-candles` expects.
/// The live cron engine pulls TradeNation candles, so a TradeNation-armed plan
/// replays against TradeNation; an OANDA plan against OANDA.
///
/// `None` for IBKR. `CandleSource` names a **candle-cache feed**, and there is
/// no IBKR one — futures candles come from a different venue with a different
/// session model. Defaulting to a CFD source would replay a futures plan
/// against a *different instrument's* prices and report the R-multiples as if
/// they were the contract's, which is worse than declining to replay.
fn source_for(broker: Broker) -> Option<CandleSource> {
    match broker {
        Broker::TradeNation => Some(CandleSource::TradeNation),
        Broker::Oanda => Some(CandleSource::Oanda),
        Broker::Ibkr => None,
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
    // Where `replay-candles` should write its positions file, when `--new-tv`
    // asked for one. `None` leaves the flag off entirely, so an invocation
    // without `--new-tv` is byte-identical to before this existed.
    positions_out: Option<&Path>,
) -> Vec<String> {
    let mut argv = vec![
        bin.to_string(),
        "--plan".to_string(),
        plan.display().to_string(),
    ];
    if !sets_flag(passthrough, "--verbose") {
        argv.push("--verbose".to_string());
    }
    // Guarded like every other default: an operator's explicit `--positions`
    // must win rather than collide with ours.
    if let Some(path) = positions_out
        && !sets_flag(passthrough, "--positions")
    {
        argv.push("--positions".to_string());
        argv.push(path.display().to_string());
    }
    // `--annotate true` is the default only for the TradingView-only path.
    // Under `--new-tv` (⇒ `positions_out` is set) local-chart is the chart, and
    // injecting the TradingView default would make the new path depend on the
    // old bridge being alive: `replay-candles` propagates the annotate error,
    // so a dead tv-mcp CDP connection exits non-zero and we never reach
    // `draw_positions_on_local_chart`. An explicit `--annotate` in the
    // passthrough still wins — asking for both charts is still possible, it is
    // just no longer automatic.
    if positions_out.is_none() && !sets_flag(passthrough, "--annotate") {
        argv.push("--annotate".to_string());
        argv.push("true".to_string());
    }
    if !sets_flag(passthrough, "--source") {
        argv.push("--source".to_string());
        argv.push(source.as_str().to_string());
    }
    // The plan's own instrument, so the replay can't inherit an unrelated
    // TradingView chart's symbol (which outranks the plan in
    // `replay-candles`' fallback chain). Guarded like every other default:
    // `ArgAction::Set` rejects a repeated flag, so an operator's explicit
    // `--instrument` must suppress this rather than collide with it.
    if let Some(instrument) = arm.instrument
        && !sets_flag(passthrough, "--instrument")
    {
        argv.push("--instrument".to_string());
        argv.push(instrument.to_string());
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
    // `--new-tv [URL]`: draw the replayed positions on local-chart INSTEAD of
    // TradingView. Passing it suppresses the `--annotate true` default, so a
    // failure in the tv-mcp bridge can no longer abort the replay before the
    // local-chart drawing runs. An explicit `--annotate` still paints both.
    new_tv: Option<&str>,
) -> Result<()> {
    let bin = replay_binary();
    let plan = plan_path(plan_out, trade_id);
    if !plan.exists() {
        return Err(eyre!(
            "--replay: plan JSON not found at {} (expected it to be written before replay)",
            plan.display()
        ));
    }
    let source = source_for(broker).ok_or_else(|| {
        eyre!(
            "--replay: no candle source for {} — replay-candles has no IBKR feed, and \
             replaying against another broker's prices would report a different \
             instrument's R-multiples",
            broker.as_str()
        )
    })?;
    // Under `--new-tv`, ask the replay to also write its positions somewhere we
    // can read them back. A temp file keyed on the trade id: it is an
    // intermediate between two processes in one run, not an artefact the
    // operator keeps.
    let positions_path =
        new_tv.map(|_| std::env::temp_dir().join(format!("tv-arm-positions-{trade_id}.json")));
    let argv = build_argv(
        &bin,
        &plan,
        source,
        passthrough,
        arm,
        positions_path.as_deref(),
    );

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

    // The replay succeeded; draw what it recorded. Only under `--new-tv` —
    // without it, nothing above wrote a positions file and this is skipped
    // entirely.
    if let (Some(url), Some(path)) = (new_tv, positions_path.as_deref()) {
        draw_positions_on_local_chart(url, path)?;
    }
    Ok(())
}

/// Read the positions `replay-candles` just wrote and draw them on
/// local-chart.
///
/// **Fail-soft**, deliberately: the plan is already armed and the replay has
/// already printed its report by the time this runs. A chart that could not be
/// drawn on is a missing picture, not a wrong answer — so it warns and returns
/// `Ok`, rather than turning a successful arm-and-replay into a non-zero exit.
/// The one thing it must not do is fail *silently*, hence the warning naming
/// the cause.
fn draw_positions_on_local_chart(url: &str, path: &Path) -> Result<()> {
    let doc = match local_chart_client::read_positions(path) {
        Ok(doc) => doc,
        Err(err) => {
            warn!(%err, path = %path.display(), "could not read the replay's positions");
            return Ok(());
        }
    };
    // Draw the not-taken brackets too: on local-chart they are cheap (muted
    // grey) and the operator asked for a replay precisely to see what the plan
    // would have done, including the entries it never got.
    match local_chart_client::draw_positions(&doc, url, true) {
        Ok(drawn) => {
            info!(drawn, url, "drew replay positions on local-chart");
            println!("drew {drawn} position(s) on local-chart");
        }
        Err(err) => warn!(%err, url, "could not draw positions on local-chart"),
    }
    std::fs::remove_file(path).ok();
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
        assert_eq!(
            source_for(Broker::TradeNation).map(CandleSource::as_str),
            Some("tradenation")
        );
        assert_eq!(
            source_for(Broker::Oanda).map(CandleSource::as_str),
            Some("oanda")
        );
    }

    /// IBKR has no candle-cache feed. Returning `None` is what stops a futures
    /// plan replaying against a CFD broker's prices and reporting the resulting
    /// R-multiples as if they were the contract's.
    #[test]
    fn ibkr_has_no_candle_source() {
        assert_eq!(source_for(Broker::Ibkr), None);
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
            None,
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
            None,
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
            None,
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
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &[],
            arm,
            None,
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--arm-")),
            "no --arm-* without --save: {argv:?}"
        );
        ReplayArgs::try_parse_from(&argv).expect("must parse");
    }

    /// The reversal axis is recorded only when the flag was actually passed.
    ///
    /// A bare `--arm-*` flag emitted unconditionally would stamp `rev-off` onto
    /// every fixture the matrix saves, collapsing both halves of the axis into
    /// the reversals-off column — a grid that looks full while measuring one
    /// variant twice.
    #[test]
    fn the_reversal_axis_is_not_recorded_when_the_flag_is_off() {
        let plan = PathBuf::from("/tmp/p.json");
        let arm = ArmContext {
            skip_reversals: false,
            ..Default::default()
        };
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--save".to_string(), "trade-124".to_string()],
            arm,
            None,
        );
        assert!(
            !argv.iter().any(|a| a == "--arm-skip-reversals"),
            "reversals ON must not be stamped as off: {argv:?}"
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("must parse");
        assert!(!parsed.arm_skip_reversals);
    }

    /// With `--save`, the variant is recorded and the whole invocation parses.
    #[test]
    fn arm_tokens_record_the_variant_alongside_save() {
        let plan = PathBuf::from("/tmp/p.json");
        let arm = ArmContext {
            skip_bcr: true,
            skip_calendar_bars: true,
            skip_golden: true,
            skip_reversals: true,
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
            None,
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("arm tokens must parse with --save");
        assert_eq!(parsed.arm_entry_rule.as_deref(), Some("skip-bcr"));
        assert!(parsed.arm_skip_calendar_bars);
        assert!(parsed.arm_skip_golden);
        assert!(
            parsed.arm_skip_reversals,
            "the reversal axis must reach the fixture's meta.json"
        );
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
            None,
        );
        let parsed = ReplayArgs::try_parse_from(&argv).unwrap();
        assert_eq!(parsed.arm_entry_rule.as_deref(), Some("strategy-v2"));
    }

    /// `--qm-entry market` gets its OWN column label, distinct from the default
    /// limit leg's.
    ///
    /// Without this the market cell records itself as plain `strategy-v2` and the
    /// grid silently averages two different entry mechanics into one column.
    #[test]
    fn qm_market_records_its_own_entry_rule() {
        let plan = PathBuf::from("/tmp/p.json");
        let arm = ArmContext {
            strategy_v2: true,
            qm_entry: Some(crate::args::QmEntry::Market),
            ..Default::default()
        };
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--save".to_string(), "t".to_string()],
            arm,
            None,
        );
        let parsed = ReplayArgs::try_parse_from(&argv).unwrap();
        assert_eq!(
            parsed.arm_entry_rule.as_deref(),
            Some("strategy-v2-qm-market")
        );
    }

    /// The DEFAULT QM leg (limit) keeps the plain `strategy-v2` label — both when
    /// `--qm-entry` is absent and when it's explicitly `limit`.
    ///
    /// Load-bearing for continuity: every `strategy-v2` fixture captured before
    /// `--qm-entry` existed froze the limit leg. If the label moved, those
    /// fixtures would sit in a column the current code never writes to.
    #[test]
    fn the_default_qm_limit_leg_keeps_the_plain_v2_label() {
        for qm_entry in [None, Some(crate::args::QmEntry::Limit)] {
            let arm = ArmContext {
                strategy_v2: true,
                qm_entry,
                ..Default::default()
            };
            assert_eq!(
                arm.entry_rule_label(),
                "strategy-v2",
                "limit is the default; {qm_entry:?} must not rename the column"
            );
        }
    }

    /// `--qm-entry` only qualifies the label when `--strategy-v2` is on.
    ///
    /// Clap enforces `requires = "strategy_v2"`, so this pairing can't be typed —
    /// but `ArmContext` is a plain struct a caller could fill in wrongly, and a
    /// label like `strategy-v2-qm-market` on an arm with no QM leg would be a
    /// silent lie in the corpus.
    #[test]
    fn qm_entry_without_strategy_v2_does_not_change_the_label() {
        let arm = ArmContext {
            strategy_v2: false,
            qm_entry: Some(crate::args::QmEntry::Market),
            ..Default::default()
        };
        assert_eq!(arm.entry_rule_label(), "normal");
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
            None,
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
            None,
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
            None,
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
            None,
        );
        assert!(
            ReplayArgs::try_parse_from(&argv).is_err(),
            "an unknown passthrough flag is caught by the shared clap parse"
        );
    }

    /// The bug: a chained replay never forwarded the instrument, so
    /// `replay-candles` fell back to **the TradingView chart's** symbol
    /// (`args.instrument` → chart → plan, `replay_candles.rs`'s
    /// `resolve_window`). A chart left on another pair silently replayed the
    /// plan against a *different instrument's prices*.
    ///
    /// Measured on AUD/NZD 2026-09-17 (`hs-aud-nzd-95167beb`, H1): the plan's
    /// levels sat at ~1.22 while the candles arrived at ~0.99, so every golden
    /// signal was declined `entry ... is outside the SL..TP range` and the
    /// replay reported 0 fills / +0.00R. With the instrument forwarded the same
    /// plan fills at 1.22464 and stops out for −1.00R. A wrong-feed replay is
    /// the same hazard `source_for` declines to take for IBKR — caught there,
    /// missed here.
    #[test]
    fn argv_forwards_the_plans_instrument() {
        let plan = PathBuf::from("/tmp/p.json");
        let arm = ArmContext {
            instrument: Some("AUD/NZD"),
            ..Default::default()
        };
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &[],
            arm,
            None,
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("must parse");
        assert_eq!(
            parsed.instrument.as_deref(),
            Some("AUD/NZD"),
            "the plan's instrument must reach replay-candles: {argv:?}"
        );
    }

    /// `--instrument` is `ArgAction::Set`, which **rejects a repeated flag**
    /// rather than taking the last value, so the injection has to stand down
    /// when the operator names one. Same trap as `--annotate`: parse the
    /// result, never just count token positions.
    #[test]
    fn operator_instrument_overrides_the_forwarded_default() {
        let plan = PathBuf::from("/tmp/p.json");
        let arm = ArmContext {
            instrument: Some("AUD/NZD"),
            ..Default::default()
        };
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--instrument".to_string(), "EUR/USD".to_string()],
            arm,
            None,
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "--instrument").count(),
            1,
            "exactly one --instrument must survive: {argv:?}"
        );
        let parsed = ReplayArgs::try_parse_from(&argv)
            .expect("an operator --instrument must not collide with the forwarded default");
        assert_eq!(parsed.instrument.as_deref(), Some("EUR/USD"));
    }

    /// The `=` form is the same override.
    #[test]
    fn operator_instrument_eq_form_also_overrides() {
        let plan = PathBuf::from("/tmp/p.json");
        let arm = ArmContext {
            instrument: Some("AUD/NZD"),
            ..Default::default()
        };
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--instrument=EUR/USD".to_string()],
            arm,
            None,
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("=-form must parse");
        assert_eq!(parsed.instrument.as_deref(), Some("EUR/USD"));
    }

    /// Unlike every `--arm-*` token, the instrument is **not** gated on
    /// `--save`: it selects the candle feed for the replay itself, so a plain
    /// `tv-arm ... replay` (no fixture) needs it just as much. That is the whole
    /// bug — the reported run had no `--save`.
    #[test]
    fn instrument_is_forwarded_without_save() {
        let plan = PathBuf::from("/tmp/p.json");
        let arm = ArmContext {
            instrument: Some("AUD/NZD"),
            ..Default::default()
        };
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &[],
            arm,
            None,
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--arm-")),
            "still no --arm-* without --save: {argv:?}"
        );
        assert!(
            argv.contains(&"--instrument".to_string()),
            "but the instrument IS forwarded: {argv:?}"
        );
        ReplayArgs::try_parse_from(&argv).expect("must parse");
    }

    /// An `ArmContext` with no instrument emits no flag, so `replay-candles`
    /// keeps its documented chart→plan fallback for any caller that genuinely
    /// has nothing to forward.
    #[test]
    fn no_instrument_emits_no_flag() {
        let plan = PathBuf::from("/tmp/p.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &[],
            ArmContext::default(),
            None,
        );
        assert!(
            !argv.contains(&"--instrument".to_string()),
            "nothing to forward ⇒ no flag: {argv:?}"
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("must parse");
        assert_eq!(parsed.instrument, None);
    }

    /// `--new-tv` makes the chained replay WRITE a positions file — that file
    /// is the only way the drawing step gets its data, so the flag reaching
    /// `replay-candles` is the whole mechanism.
    ///
    /// Parsed, not pattern-matched on tokens. This module's own history is the
    /// reason: a prior version asserted flag ordering by token position
    /// without ever parsing the result, so the test passed while every real
    /// invocation failed with "cannot be used multiple times".
    #[test]
    fn new_tv_asks_the_replay_for_a_positions_file() {
        let plan = PathBuf::from("/tmp/p.json");
        let out = PathBuf::from("/tmp/positions.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &[],
            ArmContext::default(),
            Some(&out),
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("must parse");
        assert_eq!(
            parsed.positions.as_deref(),
            Some(out.as_path()),
            "the replay must be told where to write its positions: {argv:?}"
        );
    }

    /// Without `--new-tv` the flag must be ABSENT, not empty-valued: an
    /// invocation that never asked for local-chart has to stay byte-identical
    /// to before this feature existed.
    #[test]
    fn without_new_tv_no_positions_flag_is_injected() {
        let plan = PathBuf::from("/tmp/p.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &[],
            ArmContext::default(),
            None,
        );
        assert!(
            !argv.contains(&"--positions".to_string()),
            "no --new-tv ⇒ no --positions: {argv:?}"
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("must parse");
        assert_eq!(parsed.positions, None);
    }

    /// `--positions` takes a value, and a repeated flag is a hard clap error
    /// rather than "last wins" — so an operator's explicit `--positions` must
    /// SUPPRESS ours, exactly as `--instrument` and `--annotate` do. Without
    /// this guard, `--new-tv` plus a hand-passed `--positions` would refuse to
    /// run at all.
    #[test]
    fn an_operator_positions_flag_suppresses_the_injected_one() {
        let plan = PathBuf::from("/tmp/p.json");
        let ours = PathBuf::from("/tmp/ours.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--positions".to_string(), "/tmp/theirs.json".to_string()],
            ArmContext::default(),
            Some(&ours),
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "--positions").count(),
            1,
            "exactly one --positions survives: {argv:?}"
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("must parse");
        assert_eq!(
            parsed.positions.as_deref(),
            Some(Path::new("/tmp/theirs.json")),
            "the operator's path wins"
        );
    }

    /// The `=` form of the operator's flag has to suppress ours too —
    /// `sets_flag` handles both spellings, and missing one would resurrect the
    /// "cannot be used multiple times" failure for `--positions=<path>`.
    #[test]
    fn the_equals_form_of_an_operator_positions_flag_also_suppresses_ours() {
        let plan = PathBuf::from("/tmp/p.json");
        let ours = PathBuf::from("/tmp/ours.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--positions=/tmp/theirs.json".to_string()],
            ArmContext::default(),
            Some(&ours),
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("=-form must parse");
        assert_eq!(
            parsed.positions.as_deref(),
            Some(Path::new("/tmp/theirs.json"))
        );
    }

    /// `--new-tv` means **local-chart only**: the `--annotate true` default is
    /// NOT injected.
    ///
    /// It used to be, so one replay could paint both charts while the new path
    /// was compared against the old. That coupling made the replacement
    /// hostage to the bridge it replaces: `annotate::annotate` propagates its
    /// error, so a dead tv-mcp CDP connection exits `replay-candles` non-zero
    /// **before** the positions file is drawn — losing the local-chart picture
    /// because TradingView was unreachable. The comparison is over; the new
    /// path stands alone.
    #[test]
    fn new_tv_does_not_inject_the_tradingview_annotate_default() {
        let plan = PathBuf::from("/tmp/p.json");
        let out = PathBuf::from("/tmp/positions.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &[],
            ArmContext::default(),
            Some(&out),
        );
        assert!(
            !argv.contains(&"--annotate".to_string()),
            "--new-tv ⇒ no --annotate default: {argv:?}"
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("must parse");
        assert!(!parsed.annotate, "TradingView is not drawn on");
        assert!(
            parsed.positions.is_some(),
            "local-chart's positions file is"
        );
    }

    /// Dropping the default must not take the *explicit* flag with it: an
    /// operator who wants both charts on one replay still says so, and the
    /// passthrough is forwarded untouched.
    #[test]
    fn new_tv_still_honours_an_explicit_annotate_in_the_passthrough() {
        let plan = PathBuf::from("/tmp/p.json");
        let out = PathBuf::from("/tmp/positions.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &["--annotate".to_string(), "true".to_string()],
            ArmContext::default(),
            Some(&out),
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "--annotate").count(),
            1,
            "exactly one --annotate — ArgAction::Set rejects a repeat: {argv:?}"
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("must parse");
        assert!(parsed.annotate, "the operator asked for TradingView too");
        assert!(parsed.positions.is_some(), "and local-chart alongside it");
    }

    /// Without `--new-tv` nothing changes: `--annotate true` is still the
    /// default, so every existing TradingView-only invocation is
    /// byte-identical to before.
    #[test]
    fn without_new_tv_the_annotate_default_is_untouched() {
        let plan = PathBuf::from("/tmp/p.json");
        let argv = build_argv(
            "replay-candles",
            &plan,
            CandleSource::TradeNation,
            &[],
            ArmContext::default(),
            None,
        );
        let parsed = ReplayArgs::try_parse_from(&argv).expect("must parse");
        assert!(parsed.annotate, "no --new-tv ⇒ TradingView default stands");
        assert_eq!(parsed.positions, None);
    }
}
