//! Measure every web-listed TradeNation market, checkpointing and merging safely.
use clap::Parser;
use color_eyre::Result;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()?;
    spread_baseline_gen::tradenation::run(spread_baseline_gen::tradenation::Args::parse()).await
}
