//! `tv-arm` — read a TradingView chart and arm a full reversal-trade
//! bundle (vetoes, preps, enter, close-on-reversal) plus pause/news
//! windows, both operator-drawn and auto-derived from the
//! forex-factory calendar.
//!
//! Port of `scripts/tv_arm_hs.py`. The chart-reading + classification
//! and alert-spec dispatch live here; the signing layer is delegated
//! to the `trade-control-cli` crate as a library.

// Phase 3 port in progress — each module lands ahead of its consumer
// (`pipeline.rs`), so the pure-logic modules look dead until the
// orchestrator wires them up. Remove this allow when `pipeline.rs`
// lands.
#![allow(dead_code)]

use color_eyre::eyre::Result;

mod geometry;
mod manifest;
mod pair_lines;
mod timeframe;

fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    println!("tv-arm — port of tv_arm_hs.py in progress. See plan file.");
    Ok(())
}
