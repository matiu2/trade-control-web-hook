//! Opt-in sequential demo probes; all mutations address only IDs created by this run.
use clap::Parser;
use color_eyre::Result;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[path = "ctrader-integration-check/args.rs"]
mod args;
#[path = "ctrader-integration-check/evidence.rs"]
mod evidence;
#[path = "ctrader-integration-check/geometry.rs"]
mod geometry;
#[path = "ctrader-integration-check/probes.rs"]
mod probes;
#[path = "ctrader-integration-check/verify.rs"]
mod verify;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()?;
    let args = args::Args::parse();
    probes::run(&args).await
}

#[cfg(test)]
fn test_logging() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
