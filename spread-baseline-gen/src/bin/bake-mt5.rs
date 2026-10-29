//! Bake account-pinned MT5 evidence without network or trades.
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
    /// Explicitly fill failed symbols using a separate completed report of at least five days.
    #[arg(long)]
    short_history_report: Option<PathBuf>,
    #[arg(long, default_value = "../candle-cache/src/spread_profiles/mt5.rs")]
    out: PathBuf,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer())
        .try_init()?;
    let args = Args::parse();
    let account = mt5_data_source::account_config::AccountConfig::load(&args.account)?;
    let report = serde_json::from_slice(&std::fs::read(&args.report).wrap_err("read MT5 report")?)?;
    let (table, rows) = if let Some(path) = &args.short_history_report {
        let recent =
            serde_json::from_slice(&std::fs::read(path).wrap_err("read short-history report")?)?;
        let table = spread_baseline_gen::mt5::bake::render_with_short_history(
            &report,
            &recent,
            &args.account,
            account.login,
            &account.server,
        )?;
        (table, report.instruments.len() + recent.instruments.len())
    } else {
        let table = spread_baseline_gen::mt5::bake::render(
            &report,
            &args.account,
            account.login,
            &account.server,
        )?;
        (table, report.instruments.len())
    };
    spread_baseline_gen::mt5::output::write(&args.out, &args.account, &table)?;
    tracing::info!(account = args.account, rows, short_history = args.short_history_report.is_some(), output = %args.out.display(), "baked validated MT5 spread profiles");
    Ok(())
}
