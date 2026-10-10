//! Wait for calibration, validate/bake it, then tag and rebuild staging artifacts.
use std::path::PathBuf;

use clap::Parser;
use color_eyre::Result;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    account: String,
    #[arg(long)]
    report: PathBuf,
    #[arg(long)]
    calibration_status: PathBuf,
    #[arg(long)]
    status: PathBuf,
    /// Refuse a new bake/release until every requested instrument has succeeded.
    #[arg(long)]
    require_complete: bool,
    /// Fast-forward main to the validated staging release and push it.
    #[arg(long)]
    publish_main: bool,
    /// Install staging binaries and restart the staging worker after publishing.
    #[arg(long)]
    deploy_staging: bool,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer())
        .try_init()?;
    let args = Args::parse();
    let _completion_lock = lock::CompletionLock::acquire(&args.status)?;
    run::write_status(&args, "waiting", "90-day calibration still running")?;
    if let Err(error) = run::finish(&args) {
        run::write_status(&args, "failed", &format!("{error:#}"))?;
        return Err(error);
    }
    Ok(())
}

#[path = "finish-mt5/lock.rs"]
mod lock;
#[path = "finish-mt5/release.rs"]
mod release;
#[path = "finish-mt5/run.rs"]
mod run;
#[path = "finish-mt5/promotion.rs"]
mod promotion;
