//! `tv-arm` — read a TradingView chart and arm a full reversal-trade
//! bundle (vetoes, preps, enter, close-on-reversal) plus pause/news
//! windows, both operator-drawn and auto-derived from the
//! forex-factory calendar.
//!
//! Port of `scripts/tv_arm_hs.py`. The chart-reading + classification
//! and alert-spec dispatch live here; the signing layer is delegated
//! to the `trade-control-cli` crate as a library.

// Some library-style helpers (`horizontal_price`, `Manifest::trade_id`,
// `DrawShapeResult.shape`/`.entity_id`, etc.) aren't consumed by the
// current pipeline but are public API for downstream tools; suppress
// the dead-code warnings rather than dropping useful surface.
#![allow(dead_code)]

use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use color_eyre::eyre::Result;

mod alert_spec;
mod args;
mod create_alerts;
mod drawings;
mod geometry;
mod manifest;
mod pair_lines;
mod pipeline;
mod roles;
mod timeframe;
mod tv_mcp;

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
