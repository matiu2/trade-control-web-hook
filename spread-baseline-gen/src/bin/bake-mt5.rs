//! Bake an account-pinned, completed 90-day MT5 report without network or trades.
use std::path::PathBuf;

use clap::Parser;
use color_eyre::{Result, eyre::Context};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    account: String,
    #[arg(long)]
    report: PathBuf,
    #[arg(long, default_value = "core/src/spread_baseline_mt5.rs")]
    out: PathBuf,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry().with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))).with(tracing_error::ErrorLayer::default()).with(tracing_subscriber::fmt::layer()).try_init()?;
    let args = Args::parse();
    let account = mt5_data_source::account_config::AccountConfig::load(&args.account)?;
    let report = serde_json::from_slice(&std::fs::read(&args.report).wrap_err("read MT5 report")?)?;
    let table = spread_baseline_gen::mt5::bake::render(&report, &args.account, account.login, &account.server)?;
    let temporary = args.out.with_extension("rs.tmp");
    std::fs::write(&temporary, table)?;
    std::fs::rename(&temporary, &args.out)?;
    tracing::info!(account = args.account, rows = report.instruments.len(), output = %args.out.display(), "baked validated MT5 spread profiles");
    Ok(())
}
