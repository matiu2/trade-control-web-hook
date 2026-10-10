//! Read-only calibration; writes an account-pinned JSON report, never a live table.
#[path = "generate-mt5/run.rs"]
mod mt5_calibration;
#[path = "generate-mt5/checkpoint.rs"]
mod checkpoint;

use clap::Parser;
use color_eyre::Result;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
    mt5_calibration::run(mt5_calibration::Args::parse()).await
}
