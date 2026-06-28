//! `replay-candles` — replay a historical candle window through the cron
//! engine's pure decision logic, offline.
//!
//! Given a `TradePlan` (written by `tv-arm --plan-out`) and a time window, this
//! pulls the broker candles for that window (via candle-cache), feeds them
//! through the engine's `evaluate_plan` one closed bar at a time exactly as the
//! live cron tick does, and — for each fired enter — runs the pure
//! `simulate_fill` over the forward candles to show what the broker would have
//! done. No `wrangler dev`, no HTTP, no live broker orders.
//!
//! The worker has no candle-ingest endpoint and its order-dispatch path
//! (`run_enter`) can't run off-wasm (it builds a `worker::Response` that panics
//! at construction), so this drives the *pure* engine core natively and uses the
//! fill simulator as the faithful, broker-free stand-in for execution.
//!
//! With explicit flags:
//!
//! ```text
//! replay-candles --plan plan.json --instrument eur/cad --granularity 1h \
//!   --source tradenation --start 2026-06-18T11:00
//! ```
//!
//! Or, with no window flags, the window resolves itself from the plan + the
//! live TradingView chart, for the natural replay workflow:
//!
//!   - **granularity** comes from the **plan** (`plan.granularity`).
//!   - **start** is the chart's **last shown candle** (`bars_range.to`) — in TV
//!     replay mode that's the replay cursor, so the operator just rewinds the
//!     chart to the start of the trade.
//!   - **end** is the plan's **trade-expiry** rule (`TimeReached.at_epoch`),
//!     falling back to the chart's visible-region end if the plan has none.
//!   - **instrument** falls back chart-symbol → plan.
//!
//! So the operator rewinds TradingView to the trade start and just runs
//! `replay-candles --plan plan.json`. Any flag that *is* passed overrides the
//! corresponding resolved value.

mod replay_candles {
    pub mod annotate;
    pub mod brisbane;
    pub mod candles;
    pub mod fixture;
    pub mod granularity;
    pub mod instrument;
    pub mod market_hours;
    pub mod replay;
    pub mod replay_broker;
    pub mod report;
    pub mod source;
    pub mod tv;
    pub mod verbose;
}

use std::fs;
use std::path::PathBuf;

use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use color_eyre::eyre::{Context, Result, eyre};
use tracing_error::ErrorLayer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use replay_candles::fixture::{self, FixtureMeta, ReplayOutcome};
use replay_candles::source::CandleSource;
use replay_candles::tv::TvDefaults;
use replay_candles::{
    annotate, brisbane, candles, granularity, instrument, market_hours, replay, report, tv,
};
use trade_control_engine::{BidAskCandle as EngineCandle, Granularity, TradePlan, Trigger};
use trading_view::mcp::TvMcp;

#[derive(Parser)]
#[command(name = "replay-candles")]
#[command(about = "Replay a candle window through the engine's decision logic, offline")]
struct Args {
    /// Path to the TradePlan JSON written by `tv-arm --plan-out`. Required for a
    /// live replay; omitted (and ignored) under `--test-mode`, where the plan
    /// comes from the saved fixture.
    #[arg(long)]
    plan: Option<PathBuf>,

    /// Instrument to pull candles for (e.g. `eur/cad`). Overrides the chart's
    /// symbol; falls back to the TradingView chart, then the plan's instrument.
    /// Resolved per-source via instrument-lookup.
    #[arg(long)]
    instrument: Option<String>,

    /// Candle granularity (`1m`/`5m`/`15m`/`1h`/`4h`/`1d`). Defaults to the
    /// plan's granularity; pass this only to override it (the override must
    /// still match the plan's granularity).
    #[arg(long)]
    granularity: Option<String>,

    /// Which broker candle-cache pulls from. Both sources always go through
    /// candle-cache (filling the on-disk cache either way); this only selects
    /// the broker. TradeNation matches the live engine.
    #[arg(long, value_enum, default_value_t = CandleSource::TradeNation)]
    source: CandleSource,

    /// Window start, UTC (e.g. `2026-06-18T11:00` or `2026-06-18T11:00:00`).
    /// Overrides the chart's last-shown-candle (replay cursor). Omit to read it
    /// from the TradingView chart.
    #[arg(long)]
    start: Option<String>,

    /// Window end, UTC. Overrides the plan's trade-expiry. Omit to use the
    /// plan's trade-expiry (or, if it has none, the chart's visible-region end).
    #[arg(long)]
    end: Option<String>,

    /// Override the tv-mcp module root used to read the chart when window flags
    /// are omitted. Defaults to the hard-coded `~/Downloads/tradingview-mcp-jackson`.
    #[arg(long)]
    tv_mcp_root: Option<PathBuf>,

    /// Run the fill simulator on each fired enter (default on).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    simulate: bool,

    /// Print a bar-by-bar trace of the engine's silent state changes before the
    /// fire report: phase transitions, the break-and-close stamp, and the
    /// **retest stamp** (which never fires an intent, so it's invisible in the
    /// normal report). Quiet bars are omitted. For debugging "why did/didn't the
    /// entry fire" — it shows exactly which bar armed the retest gate.
    #[arg(long, visible_alias = "all-events", default_value_t = false)]
    verbose: bool,

    /// After replaying, draw each *filled* position onto the live TradingView
    /// chart (two rectangles per trade: entry→TP green, entry→SL red), spanning
    /// the fill bar to the exit. Prior `--annotate` drawings are cleared first;
    /// your hand-drawn necklines/fibs are left alone. Implies `--simulate`
    /// (annotation needs the simulated fill). Uses the same tv-mcp chart as
    /// window resolution (`--tv-mcp-root`).
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    annotate: bool,

    /// Also annotate *not-taken* trades — pending orders that never filled and
    /// entries the worker declined — as muted grey boxes at the fire bar. Only
    /// meaningful with `--annotate` (and implies it). Off by default, so a
    /// plain `--annotate` shows just the taken positions.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    annotate_unfilled: bool,

    /// Number of extra candles pulled *before* the window start as a silent
    /// warm-up prefix. These bars seed the detector (so ATR is warm and the
    /// candle patterns have context) and prime the FSM, but fire nothing — the
    /// plan only goes live at the window start. Without this, a `needs_golden`
    /// enter can never fire (ATR never warms) and a stale veto-level touch in
    /// the warm-up span would wrongly retire the plan. 200 covers the 96-bar
    /// 15m ATR plus pattern lookback; raise it for very long-lookback configs.
    #[arg(long, default_value_t = 200)]
    warmup_bars: usize,

    /// Override the candle-cache disk cache directory.
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Print the zsh completion script to stdout and exit. Source it into your
    /// fpath (or `source <(replay-candles --print-completions)`).
    #[arg(long)]
    print_completions: bool,

    /// After a live replay, freeze this run's inputs (plan + the pulled candle
    /// window + resolved meta) and its outcome into `<fixtures-dir>/<NAME>/`, a
    /// golden regression case the test suite re-runs offline. Run it once a
    /// replay is producing the verdict you want.
    #[arg(long, value_name = "NAME")]
    save: Option<String>,

    /// Replay a saved fixture **offline**: load plan + candles + meta from
    /// `<fixtures-dir>/<--fixture>/` instead of pulling from the broker (no
    /// network, no env vars, no TradingView). Requires `--fixture`.
    #[arg(long, requires = "fixture")]
    test_mode: bool,

    /// Name of the fixture under `<fixtures-dir>/` to replay with `--test-mode`.
    #[arg(long, value_name = "NAME")]
    fixture: Option<String>,

    /// Under `--test-mode`, also compare the replay's outcome against the
    /// fixture's `expected.json` and exit non-zero on any mismatch (printing the
    /// diff). The gate proof for a fixture.
    #[arg(long)]
    check: bool,

    /// Directory holding the saved fixtures. Defaults to `replay-fixtures` at the
    /// repo root (relative to the cli crate's manifest).
    #[arg(long)]
    fixtures_dir: Option<PathBuf>,
}

/// Default fixtures directory: `<repo-root>/replay-fixtures`, resolved from the
/// cli crate's manifest dir (`.../cli`) so it's stable regardless of cwd.
fn default_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("replay-fixtures")
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    // Handle completions before clap's required-arg validation: `--plan` is
    // required, so a plain `Args::parse()` would reject a bare
    // `--print-completions`. Detect it on the raw argv first, emit, and exit.
    if std::env::args().any(|a| a == "--print-completions") {
        print_completions();
        return Ok(());
    }

    init_tracing();

    let args = Args::parse();

    // `--test-mode` is a fully-offline branch: no broker, no TradingView, no
    // env vars — everything comes from the saved fixture.
    if args.test_mode {
        return run_test_mode(&args).await;
    }

    // `--annotate-unfilled` is a superset of `--annotate` (it adds the
    // not-taken trades), so it implies annotation is on.
    let annotate = args.annotate || args.annotate_unfilled;
    // Annotation draws each position, which needs the simulated fill — so
    // annotating forces simulation on even if the operator passed
    // `--simulate false`.
    let simulate = args.simulate || annotate;
    if annotate && !args.simulate {
        tracing::info!("annotation implies --simulate; running the fill simulator");
    }

    let plan_path = args
        .plan
        .clone()
        .ok_or_else(|| eyre!("--plan is required (or use --test-mode --fixture <name>)"))?;
    let plan = load_plan(&plan_path)?;

    // Granularity comes from the plan; `--granularity` only overrides, and an
    // override must still match the plan (a mismatch would replay the wrong
    // bars). `gran_label` is the friendly form for logging / errors.
    let gran = resolve_granularity(&args, &plan)?;
    let gran_label = granularity::engine_label(plan.granularity);

    let window = resolve_window(&args, &plan)?;

    let raw_instrument = window.instrument.as_deref().unwrap_or(&plan.instrument);
    let symbol = instrument::resolve_for(raw_instrument, args.source)?;

    let start = window.start;
    let end = window.end;
    if end <= start {
        return Err(eyre!("end ({end}) must be after start ({start})"));
    }

    // The engine evaluates a `TimeReached` (trade-expiry) against each candle's
    // *open* time, so a trade-expiry at `end` only fires once a bar *opens* at
    // or after `end` — one bar past it. Pull that extra bar so the expiry
    // actually fires (without it the window stops one bar short and the plan
    // never retires). Harmless when there's no expiry: the engine stops at the
    // first `done`, and trailing candles are ignored.
    let pull_end = end + Duration::seconds(gran.engine().seconds());

    // Pull a silent warm-up prefix before `start`: these bars seed the detector
    // (warm ATR, pattern context) and the FSM but fire nothing — the plan goes
    // live at `start` (see `replay::run`'s `live_start`). The cache pull is
    // time-windowed, so size the prefix by time = warmup_bars × bar.
    let bar_secs = gran.engine().seconds();
    let pull_from = start - Duration::seconds(bar_secs * args.warmup_bars as i64);

    tracing::info!(
        instrument = %symbol,
        granularity = %gran_label,
        source = ?args.source,
        warmup_from = %brisbane::bne(pull_from),
        start = %brisbane::bne(start),
        end = %brisbane::bne(end),
        pull_end = %brisbane::bne(pull_end),
        warmup_bars = args.warmup_bars,
        "pulling candles (times in Brisbane, UTC+10)"
    );
    let candles = candles::pull(
        args.source,
        &symbol,
        gran,
        pull_from,
        pull_end,
        args.cache_dir.clone(),
    )
    .await?;
    if candles.is_empty() {
        return Err(eyre!(
            "no candles returned for {symbol} {gran_label} in [{pull_from}, {pull_end}]"
        ));
    }
    let warmup_count = candles.iter().filter(|c| c.time < start).count();
    tracing::info!(
        count = candles.len(),
        warmup = warmup_count,
        live = candles.len() - warmup_count,
        "pulled candles"
    );

    // Keep the state TTL past the window so nothing expires mid-replay.
    let expires_at = end + Duration::days(365);
    let replay = replay::run(&plan, &candles, gran.engine(), start, expires_at).await;

    // Market-hours no-entry windows (for the blackout sweep reason). Source
    // pending — currently empty + WARN; see `market_hours`. Fail-soft.
    let blackout_windows =
        market_hours::resolve_blackout_windows(args.source, raw_instrument).await;

    print!(
        "{}",
        report::render(&plan, &replay, simulate, args.verbose, &blackout_windows)
    );

    if annotate {
        let mcp = match &args.tv_mcp_root {
            Some(root) => TvMcp::new(root.clone()),
            None => TvMcp::default(),
        };
        let scope = if args.annotate_unfilled {
            "positions (incl. not-taken)"
        } else {
            "filled positions"
        };
        tracing::info!(root = %mcp.root().display(), "annotating {scope} on the chart");
        let drawn = annotate::annotate(&mcp, &plan, &replay, args.annotate_unfilled)?;
        println!("annotated {drawn} position(s) on the chart");
    }

    if let Some(name) = &args.save {
        let meta = FixtureMeta {
            instrument: symbol.clone(),
            granularity: gran.engine(),
            source: args.source,
            start,
            end,
        };
        let expected = ReplayOutcome::compute(&plan, &replay, simulate);
        let dir = fixtures_dir(&args).join(name);
        fixture::save(&dir, &plan, &candles, &meta, &expected)?;
        tracing::info!(dir = %dir.display(), "saved fixture");
    }

    Ok(())
}

/// The fixtures directory: `--fixtures-dir` if given, else the repo-root default.
fn fixtures_dir(args: &Args) -> PathBuf {
    args.fixtures_dir
        .clone()
        .unwrap_or_else(default_fixtures_dir)
}

/// Replay a saved fixture offline. Loads plan + candles + meta from the fixture
/// dir, runs the pure engine over the frozen candles, prints the report, and —
/// under `--check` — diffs the computed outcome against `expected.json`,
/// returning an error (non-zero exit) on any mismatch.
async fn run_test_mode(args: &Args) -> Result<()> {
    let name = args
        .fixture
        .as_deref()
        .ok_or_else(|| eyre!("--test-mode requires --fixture <name>"))?;
    let dir = fixtures_dir(args).join(name);
    tracing::info!(dir = %dir.display(), "replaying fixture offline");

    let inputs = fixture::load(&dir)?;
    let replay = run_frozen(
        &inputs.plan,
        &inputs.candles,
        inputs.meta.granularity,
        inputs.meta.start,
    )
    .await;

    // A saved-fixture replay has no live instrument to resolve hours for, so the
    // blackout sweep reason isn't reconstructed here (empty windows). Fixtures
    // froze their verdict before this feature, so this keeps them byte-stable.
    print!(
        "{}",
        report::render(&inputs.plan, &replay, args.simulate, args.verbose, &[])
    );

    if args.check {
        let computed = ReplayOutcome::compute(&inputs.plan, &replay, args.simulate);
        let expected = fixture::load_expected(&dir)?;
        if computed != expected {
            return Err(diff_error(&expected, &computed));
        }
        tracing::info!("fixture matches expected.json");
    }
    Ok(())
}

/// Run the pure engine over a frozen candle window. Mirrors the live path's
/// `replay::run` call, with a far-future TTL so nothing expires mid-replay (the
/// window's own end isn't needed — the candles are fixed). `live_start` is the
/// saved window start: frozen candles include the warm-up prefix pulled before
/// it, so the plan goes live at `live_start` exactly as it did at save time.
async fn run_frozen(
    plan: &TradePlan,
    candles: &[EngineCandle],
    gran: Granularity,
    live_start: DateTime<Utc>,
) -> replay::Replay {
    let expires_at = candles.last().map(|c| c.time).unwrap_or_else(Utc::now) + Duration::days(365);
    replay::run(plan, candles, gran, live_start, expires_at).await
}

/// Build a readable diff error when a fixture's computed outcome diverges from
/// its `expected.json` — the two pretty-printed JSON blobs, side by side.
fn diff_error(expected: &ReplayOutcome, got: &ReplayOutcome) -> color_eyre::eyre::Report {
    let exp = serde_json::to_string_pretty(expected).unwrap_or_default();
    let act = serde_json::to_string_pretty(got).unwrap_or_default();
    eyre!(
        "fixture outcome does not match expected.json\n--- expected ---\n{exp}\n--- got ---\n{act}"
    )
}

/// The fully-resolved replay window: instrument (or `None` to fall back to the
/// plan) and the UTC start/end. Granularity is resolved separately (from the
/// plan, see [`resolve_granularity`]).
struct ResolvedWindow {
    instrument: Option<String>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

/// Resolve the granularity. Defaults to the plan's granularity; `--granularity`
/// only overrides, and the override must still match the plan (a mismatch would
/// replay the wrong bars through a detector configured for a different bar
/// size, so we refuse it).
fn resolve_granularity(args: &Args, plan: &TradePlan) -> Result<granularity::ReplayGranularity> {
    let plan_label = granularity::engine_label(plan.granularity);
    let Some(raw) = &args.granularity else {
        // No override: take the plan's granularity straight.
        return granularity::parse(plan_label);
    };
    let gran = granularity::parse(raw)?;
    if gran.engine() != plan.granularity {
        return Err(eyre!(
            "granularity {raw} does not match the plan's granularity {plan_label} — \
             drop --granularity to use the plan's, or pass --granularity {plan_label}"
        ));
    }
    Ok(gran)
}

/// Resolve the replay window from flags, the plan, and TradingView. Precedence,
/// per field:
///
///   - **start** — `--start` flag → chart's last shown candle (replay cursor).
///   - **end** — `--end` flag → plan's trade-expiry → chart visible-region end.
///   - **instrument** — `--instrument` flag → chart symbol → (caller) plan.
///
/// TradingView is consulted only when something it provides is actually needed:
/// the start cursor, the symbol, or the end-fallback (and the end-fallback is
/// only reached when the plan has no trade-expiry rule). So a fully-flagged
/// window, or one whose end comes from the plan, needs no MCP call.
fn resolve_window(args: &Args, plan: &TradePlan) -> Result<ResolvedWindow> {
    let plan_expiry = trade_expiry_epoch(plan).and_then(|at| Utc.timestamp_opt(at, 0).single());

    // The chart is needed for the start cursor, the symbol, or (only when the
    // plan has no expiry and no --end) the end fallback.
    let need_end_from_chart = args.end.is_none() && plan_expiry.is_none();
    let need_tv = args.start.is_none() || args.instrument.is_none() || need_end_from_chart;

    let tv = if need_tv {
        let mcp = match &args.tv_mcp_root {
            Some(root) => TvMcp::new(root.clone()),
            None => TvMcp::default(),
        };
        tracing::info!(
            root = %mcp.root().display(),
            "reading replay defaults from TradingView chart"
        );
        Some(tv::pull_defaults(&mcp)?)
    } else {
        None
    };

    let instrument = args
        .instrument
        .clone()
        .or_else(|| tv.as_ref().map(|d: &TvDefaults| d.instrument.clone()));

    let start = match (&args.start, &tv) {
        (Some(s), _) => parse_utc(s).wrap_err("parse --start")?,
        (None, Some(d)) => d.start,
        (None, None) => unreachable!("need_tv is true when --start is absent"),
    };

    let end = match (&args.end, plan_expiry, &tv) {
        (Some(e), _, _) => parse_utc(e).wrap_err("parse --end")?,
        (None, Some(expiry), _) => expiry,
        (None, None, Some(d)) => d.fallback_end,
        (None, None, None) => {
            unreachable!("need_end_from_chart is true when --end and plan expiry are both absent")
        }
    };

    Ok(ResolvedWindow {
        instrument,
        start,
        end,
    })
}

/// Pull the plan's trade-expiry as a Unix epoch (seconds, UTC), if it has one.
/// The expiry is a [`Trigger::TimeReached`] rule whose `rule_id` contains
/// `trade-expiry` (e.g. `02-veto-trade-expiry`) — the same id the engine keys
/// on. Returns `None` for a plan with no such rule (the caller then falls back
/// to the chart's visible-region end).
fn trade_expiry_epoch(plan: &TradePlan) -> Option<i64> {
    plan.rules.iter().find_map(|rule| {
        if !rule.rule_id.contains("trade-expiry") {
            return None;
        }
        match rule.trigger {
            Trigger::TimeReached { at_epoch } => Some(at_epoch),
            _ => None,
        }
    })
}

fn load_plan(path: &PathBuf) -> Result<TradePlan> {
    let text =
        fs::read_to_string(path).wrap_err_with(|| format!("read plan {}", path.display()))?;
    serde_json::from_str(&text).wrap_err_with(|| format!("parse plan JSON {}", path.display()))
}

/// Parse a naive datetime string as UTC. Accepts both minute and second
/// precision (`...T11:00` and `...T11:00:00`).
fn parse_utc(s: &str) -> Result<DateTime<Utc>> {
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(Utc.from_utc_datetime(&naive));
        }
    }
    Err(eyre!(
        "{s:?} is not a valid UTC datetime (expected YYYY-MM-DDTHH:MM[:SS])"
    ))
}

/// Emit the clap-generated zsh completion script. Binds the completion to the
/// invoked binary name (argv[0] stem) so a renamed-on-install copy emits
/// completions for its own name, falling back to the clap command name. Mirrors
/// the `tv-arm --print-completions` pattern.
fn print_completions() {
    let mut cmd = Args::command();
    let name = std::env::args()
        .next()
        .and_then(|a| {
            std::path::Path::new(&a)
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| cmd.get_name().to_string());
    generate(Shell::Zsh, &mut cmd, name, &mut std::io::stdout());
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(ErrorLayer::default())
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_engine::Granularity;

    #[test]
    fn parses_minute_and_second_precision() {
        let a = parse_utc("2026-06-18T11:00").unwrap();
        let b = parse_utc("2026-06-18T11:00:00").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, Utc.with_ymd_and_hms(2026, 6, 18, 11, 0, 0).unwrap());
    }

    #[test]
    fn rejects_garbage_datetime() {
        assert!(parse_utc("yesterday").is_err());
    }

    /// Build a minimal `TradePlan` from JSON, with the given rules spliced in.
    /// Plans are loaded from JSON in the real flow, so exercising serde here
    /// also confirms the rule shapes the resolver reads match the wire form.
    fn plan_with_rules(granularity: &str, rules_json: &str) -> TradePlan {
        let json = format!(
            r#"{{
                "trade_id": "test-1",
                "instrument": "EUR_USD",
                "direction": "long",
                "granularity": "{granularity}",
                "pip_size": 0.0001,
                "rules": {rules_json}
            }}"#
        );
        serde_json::from_str(&json).expect("parse test plan")
    }

    /// A single rule JSON with the given id + a `TimeReached` trigger. The
    /// intent is the minimal set of non-defaulted `Intent` fields.
    fn time_rule(rule_id: &str, at_epoch: i64) -> String {
        format!(
            r#"{{
                "rule_id": "{rule_id}",
                "trigger": {{ "type": "time_reached", "at_epoch": {at_epoch} }},
                "fire_mode": "once",
                "intent": {{
                    "v": 1,
                    "id": "{rule_id}-intent",
                    "not_after": "2027-01-01T00:00:00Z",
                    "action": "veto",
                    "instrument": "EUR_USD"
                }}
            }}"#
        )
    }

    #[test]
    fn extracts_trade_expiry_epoch() {
        let expiry = Utc
            .with_ymd_and_hms(2026, 6, 16, 15, 0, 0)
            .unwrap()
            .timestamp();
        let rules = format!("[{}]", time_rule("02-veto-trade-expiry", expiry));
        let plan = plan_with_rules("h1", &rules);
        assert_eq!(trade_expiry_epoch(&plan), Some(expiry));
    }

    #[test]
    fn ignores_non_expiry_time_rules() {
        // A plan with a time rule that isn't the trade-expiry (a pause window)
        // has no recoverable expiry.
        let rules = format!("[{}]", time_rule("pause-start-news1", 1_780_000_000));
        let plan = plan_with_rules("h1", &rules);
        assert_eq!(trade_expiry_epoch(&plan), None);
    }

    #[test]
    fn no_rules_means_no_expiry() {
        let plan = plan_with_rules("h1", "[]");
        assert_eq!(trade_expiry_epoch(&plan), None);
    }

    #[test]
    fn granularity_defaults_to_plan() {
        let plan = plan_with_rules("h1", "[]");
        let args = base_args();
        let gran = resolve_granularity(&args, &plan).unwrap();
        assert_eq!(gran.engine(), Granularity::H1);
    }

    #[test]
    fn granularity_override_matching_plan_is_accepted() {
        let plan = plan_with_rules("h1", "[]");
        let mut args = base_args();
        args.granularity = Some("1h".into());
        assert_eq!(
            resolve_granularity(&args, &plan).unwrap().engine(),
            Granularity::H1
        );
    }

    #[test]
    fn granularity_override_mismatching_plan_is_rejected() {
        let plan = plan_with_rules("h1", "[]");
        let mut args = base_args();
        args.granularity = Some("5m".into());
        let err = resolve_granularity(&args, &plan).unwrap_err().to_string();
        assert!(err.contains("does not match"), "got: {err}");
    }

    /// `Args` with only `--plan` set; the rest at their defaults. Lets the
    /// resolver tests flip individual flags.
    fn base_args() -> Args {
        Args {
            plan: Some(PathBuf::from("unused.json")),
            instrument: None,
            granularity: None,
            source: CandleSource::TradeNation,
            start: None,
            end: None,
            tv_mcp_root: None,
            simulate: true,
            verbose: false,
            annotate: false,
            annotate_unfilled: false,
            warmup_bars: 200,
            cache_dir: None,
            print_completions: false,
            save: None,
            test_mode: false,
            fixture: None,
            check: false,
            fixtures_dir: None,
        }
    }
}
