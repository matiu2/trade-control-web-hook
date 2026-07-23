//! `journal` — a keyboard-first TUI to walk old `trade-control` plans, load
//! them into TradingView, replay them, and delete once journalled.
//!
//! Environment-suffixed like `trade-control` / `tv-arm`: `journal-staging`
//! drives `trade-control-staging` + `replay-candles-staging` (see `build.rs`).

mod cli;
mod plan;

use clap::Parser;
use color_eyre::eyre::Result;

/// The baked git version (see `build.rs`).
const VERSION: &str = env!("GIT_VERSION");

#[derive(Parser)]
#[command(name = "journal", version = VERSION, about = "Journal old trade-control plans")]
struct Args {
    /// Fetch + parse the plan list and print it to stderr, then exit — no TUI.
    /// A smoke test for the CLI wiring and parsers.
    #[arg(long)]
    dump: bool,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    init_tracing();

    let args = Args::parse();
    if args.dump {
        return dump_plans();
    }

    // TUI comes next; for now the entry point is the --dump smoke test.
    eprintln!("journal {VERSION} — run with --dump for now (TUI WIP)");
    Ok(())
}

/// Fetch `plan list`, parse it, and print a compact table to stderr.
fn dump_plans() -> Result<()> {
    let yaml = cli::plan_list_yaml()?;
    let rows = plan::parse_plan_list(&yaml)?;
    eprintln!("{} plan(s):", rows.len());
    for r in &rows {
        eprintln!(
            "  {:32} {:16} {:6} {:24} {}",
            r.trade_id,
            r.instrument,
            r.granularity,
            r.phase.as_deref().unwrap_or("-"),
            if r.is_archived() { "ARCHIVED" } else { "" },
        );
    }
    Ok(())
}

/// Standard tracing init with an env-filter and the error layer.
fn init_tracing() {
    use tracing_error::ErrorLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(ErrorLayer::default())
        .init();
}
