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
//!   - **start** is the plan's baked `replay_start` (from `tv-arm --start`) when
//!     present, else the chart's **last shown candle** (`bars_range.to`) — in TV
//!     replay mode that's the replay cursor, so an arm without `--start` still
//!     works by rewinding the chart to the trade start. A plan armed *with*
//!     `--start` carries its own cursor, so the chart position no longer matters.
//!   - **end** is the plan's **trade-expiry** rule (`TimeReached.at_epoch`),
//!     falling back to the chart's visible-region end if the plan has none.
//!   - **instrument** falls back chart-symbol → plan.
//!
//! So the operator rewinds TradingView to the trade start and just runs
//! `replay-candles --plan plan.json`. Any flag that *is* passed overrides the
//! corresponding resolved value.

use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use color_eyre::eyre::Result;
use tracing_error::ErrorLayer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};
use trade_control_cli::replay_args::ReplayArgs as Args;
use trade_control_cli::replay_candles::outcome;

/// Classify whatever [`run`] returned, always print a terminal line, and exit
/// with a code that tells a driver what to do about it.
///
/// `main` deliberately returns `()` rather than `Result`: letting eyre print
/// and exit(1) makes every failure look the same, and — worse — prints
/// *nothing* on the summary line a batch driver scrapes. A run that died at
/// startup was then indistinguishable from one that ran and found no trade.
/// See `replay_candles::outcome`.
#[tokio::main]
async fn main() {
    // `--json` owns stdout: the failure line below would append trailing text to
    // the JSON document and make it unparseable, which is the *same* ambiguity
    // this machinery exists to remove — a driver that can't parse the output is
    // back to guessing. Under `--json` the failure is already reported IN the
    // JSON (`ok: false` + `error` per row, `failed` in the roll-up), so the text
    // line is redundant as well as harmful. Sniffed from raw argv because a
    // failure can predate the clap parse.
    //
    // Suppressing the line is only safe because `--json` now `requires =
    // "test_mode"` and BOTH test-mode paths print their JSON *before* returning
    // the error that sets the exit code (`run_test_mode_single` at the `--check`
    // step, `run_test_mode_batch` before its `failed > 0` raise). So "exit != 0
    // with `--json`" still comes with a document on stdout. If you add a
    // test-mode failure that can return `Err` *before* its `println!`, this
    // suppression turns it into zero bytes — emit a row there instead.
    let json_mode = std::env::args().any(|a| a == "--json");
    match run().await {
        Ok(()) => std::process::exit(outcome::EXIT_OK),
        Err(report) => {
            let kind = outcome::FailureKind::classify(&report);
            // The human-readable chain first (stderr), then the machine-readable
            // terminal line (stdout, where the summary it mirrors goes).
            eprintln!("Error: {report:?}");
            if !json_mode {
                println!(
                    "{}",
                    outcome::FailureLine {
                        kind,
                        detail: &report.to_string(),
                    }
                );
            }
            std::process::exit(kind.exit_code());
        }
    }
}

async fn run() -> Result<()> {
    color_eyre::install()?;
    if std::env::args().any(|a| a == "--print-completions") {
        print_completions();
        return Ok(());
    }
    init_tracing();
    trade_control_cli::replay_driver::run_cli(Args::parse()).await
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
