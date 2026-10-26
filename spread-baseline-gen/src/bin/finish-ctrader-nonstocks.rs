//! Durable missing-only non-stock calibration completion; never submits broker orders.
use clap::Parser;
use color_eyre::{Result, eyre::Context};
use std::path::{Path, PathBuf};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
#[path = "finish-ctrader-nonstocks/args.rs"]
mod args;
#[path = "finish-ctrader-nonstocks/commands.rs"]
mod commands;
#[path = "finish-ctrader-nonstocks/scope.rs"]
mod scope;
#[path = "finish-ctrader/source.rs"]
mod source;
#[path = "finish-ctrader/status.rs"]
mod status;
#[path = "finish-ctrader/steps.rs"]
mod steps;
#[path = "finish-ctrader-nonstocks/workflow.rs"]
mod workflow;

fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()?;
    let mut args = args::Args::parse();
    args.validate()?;
    args.workspace = args.workspace.canonicalize()?;
    args.report = absolute(&args.report)?;
    args.status = absolute(&args.status)?;
    let status = status::Status::acquire(&args.report, &args.status)?;
    if let Err(error) = workflow::finish(&args, &status) {
        status
            .write("failed", &format!("{error:#}"))
            .wrap_err_with(|| format!("completion failed: {error:#}"))?;
        return Err(error);
    }
    Ok(())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.into()
    } else {
        std::env::current_dir()?.join(path)
    })
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
