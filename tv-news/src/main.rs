//! `tv-news` — annotate the active TradingView chart with vertical-line
//! pairs for upcoming forex-factory news events that affect the chart's
//! instrument.
//!
//! Operator workflow: open a chart, scroll to a 2.5–3 week window
//! around the trade idea, run `tv-news`. The tool reads the visible
//! range, resolves the chart's symbol via `instrument-lookup`, fetches
//! 2-star + 3-star events for the asset's news currencies (plus 3-star
//! USD events globally so FOMC is always annotated), de-duplicates
//! against existing chart drawings, then draws `news-start` /
//! `news-end` pairs. The downstream `tv_extract_*_trade.py` scripts
//! read those bars when annotating a past trade.

#![allow(dead_code)]

use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use color_eyre::eyre::Result;

mod args;
mod filter;
mod pipeline;

use crate::args::Args;

fn main() -> Result<ExitCode> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let parsed = Args::parse();

    if parsed.print_completions {
        let mut cmd = Args::command();
        let name = cmd.get_name().to_string();
        generate(Shell::Zsh, &mut cmd, name, &mut std::io::stdout());
        return Ok(ExitCode::SUCCESS);
    }

    let code = pipeline::run(parsed)?;
    Ok(ExitCode::from(code as u8))
}
