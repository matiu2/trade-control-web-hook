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
//! Or, with no window flags, the instrument + granularity + start/end are
//! read straight off the current TradingView chart (the replay-mode visible
//! region) via the same MCP wrapper `tv-arm` uses — so the operator scrubs the
//! chart to the trade's end and just runs `replay-candles --plan plan.json`.
//! Any flag that *is* passed overrides the corresponding chart value.

mod replay_candles {
    pub mod candles;
    pub mod granularity;
    pub mod instrument;
    pub mod replay;
    pub mod report;
    pub mod source;
    pub mod tv;
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

use replay_candles::source::CandleSource;
use replay_candles::tv::TvDefaults;
use replay_candles::{candles, granularity, instrument, replay, report, tv};
use trade_control_engine::TradePlan;
use trading_view::mcp::TvMcp;

#[derive(Parser)]
#[command(name = "replay-candles")]
#[command(about = "Replay a candle window through the engine's decision logic, offline")]
struct Args {
    /// Path to the TradePlan JSON written by `tv-arm --plan-out`.
    #[arg(long)]
    plan: PathBuf,

    /// Instrument to pull candles for (e.g. `eur/cad`). Overrides the chart's
    /// symbol; falls back to the TradingView chart, then the plan's instrument.
    /// Resolved per-source via instrument-lookup.
    #[arg(long)]
    instrument: Option<String>,

    /// Candle granularity (`1m`/`5m`/`15m`/`1h`/`4h`/`1d`). Overrides the
    /// chart's resolution. Must match the plan's granularity.
    #[arg(long)]
    granularity: Option<String>,

    /// Candle source. TradeNation matches the live engine; OANDA is disk-cached.
    #[arg(long, value_enum, default_value_t = CandleSource::TradeNation)]
    source: CandleSource,

    /// Window start, UTC (e.g. `2026-06-18T11:00` or `2026-06-18T11:00:00`).
    /// Overrides the chart's visible-region start. Omit to read it from the
    /// TradingView chart.
    #[arg(long)]
    start: Option<String>,

    /// Window end, UTC. Overrides the chart's visible-region end. Omit to read
    /// it from the TradingView chart.
    #[arg(long)]
    end: Option<String>,

    /// Override the tv-mcp module root used to read the chart when window flags
    /// are omitted. Defaults to the hard-coded `~/Downloads/tradingview-mcp-jackson`.
    #[arg(long)]
    tv_mcp_root: Option<PathBuf>,

    /// Run the fill simulator on each fired enter (default on).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    simulate: bool,

    /// Override the candle-cache disk cache directory.
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Print the zsh completion script to stdout and exit. Source it into your
    /// fpath (or `source <(replay-candles --print-completions)`).
    #[arg(long)]
    print_completions: bool,
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
    let plan = load_plan(&args.plan)?;

    let window = resolve_window(&args)?;
    let gran = granularity::parse(&window.granularity)?;

    // The plan's granularity drives the engine's detector config and the candle
    // feed; a mismatch would replay the wrong bars, so refuse it.
    if gran.engine() != plan.granularity {
        return Err(eyre!(
            "granularity {} does not match the plan's granularity {} — \
             pass --granularity {}",
            window.granularity,
            granularity::engine_label(plan.granularity),
            granularity::engine_label(plan.granularity),
        ));
    }

    let raw_instrument = window.instrument.as_deref().unwrap_or(&plan.instrument);
    let symbol = instrument::resolve_for(raw_instrument, args.source)?;

    let start = window.start;
    let end = window.end;
    if end <= start {
        return Err(eyre!("end ({end}) must be after start ({start})"));
    }

    tracing::info!(
        instrument = %symbol,
        granularity = %window.granularity,
        source = ?args.source,
        %start,
        %end,
        "pulling candles"
    );
    let candles = candles::pull(args.source, &symbol, gran, start, end, args.cache_dir).await?;
    if candles.is_empty() {
        return Err(eyre!(
            "no candles returned for {symbol} {} in [{start}, {end}]",
            window.granularity
        ));
    }
    tracing::info!(count = candles.len(), "pulled candles");

    // Keep the state TTL past the window so nothing expires mid-replay.
    let expires_at = end + Duration::days(365);
    let replay = replay::run(&plan, &candles, expires_at);

    print!("{}", report::render(&plan, &replay, args.simulate));
    Ok(())
}

/// The fully-resolved replay window: instrument (or `None` to fall back to the
/// plan), the friendly granularity string, and the UTC start/end. Built from
/// CLI flags, with any omitted field pulled from the live TradingView chart.
struct ResolvedWindow {
    instrument: Option<String>,
    granularity: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

/// Resolve the replay window from flags, consulting TradingView only when a
/// window field is missing. Explicit flags always win; the chart fills the
/// gaps. Instrument falls back to `None` here (the caller then uses the plan's
/// instrument) — the chart symbol is only consulted when TV is needed for some
/// *other* field anyway, so a bare `--plan` with all-explicit window still
/// honours the plan instrument without an MCP call.
fn resolve_window(args: &Args) -> Result<ResolvedWindow> {
    let need_tv = args.instrument.is_none()
        || args.granularity.is_none()
        || args.start.is_none()
        || args.end.is_none();

    let tv = if need_tv {
        let mcp = match &args.tv_mcp_root {
            Some(root) => TvMcp::new(root.clone()),
            None => TvMcp::default(),
        };
        tracing::info!(
            root = %mcp.root().display(),
            "reading replay window from TradingView chart"
        );
        Some(tv::pull_defaults(&mcp)?)
    } else {
        None
    };

    let instrument = args
        .instrument
        .clone()
        .or_else(|| tv.as_ref().map(|d: &TvDefaults| d.instrument.clone()));

    let granularity = match (&args.granularity, &tv) {
        (Some(g), _) => g.clone(),
        (None, Some(d)) => d.granularity.clone(),
        (None, None) => unreachable!("need_tv is true when --granularity is absent"),
    };

    let start = match (&args.start, &tv) {
        (Some(s), _) => parse_utc(s).wrap_err("parse --start")?,
        (None, Some(d)) => d.start,
        (None, None) => unreachable!("need_tv is true when --start is absent"),
    };

    let end = match (&args.end, &tv) {
        (Some(e), _) => parse_utc(e).wrap_err("parse --end")?,
        (None, Some(d)) => d.end,
        (None, None) => unreachable!("need_tv is true when --end is absent"),
    };

    Ok(ResolvedWindow {
        instrument,
        granularity,
        start,
        end,
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
}
