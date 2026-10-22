//! Verify a completed paired report against the current demo identity, then bake.
use broker_ctrader::{access::Profile, market_data::ChartData};
use clap::Parser;
use color_eyre::{Result, eyre::ensure};
use spread_baseline_gen::ctrader::{self, Report};
use std::path::PathBuf;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    account: String,
    #[arg(long)]
    report: PathBuf,
    #[arg(long, default_value = "../candle-cache/src/spread_profiles/ctrader.rs")]
    out: PathBuf,
    /// Explicitly promote eligible measured pairs while keeping failures uncovered in the report.
    #[arg(long)]
    allow_partial: bool,
}
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
    let args = Args::parse();
    let profile = Profile::load(&args.account)?;
    ctrader::generate::demo_config(&profile)?;
    let data = ChartData::connect_profile(&args.account, profile).await?;
    let identity = ctrader::generate::identity(&args.account, &data).await?;
    let report: Report = serde_json::from_slice(&tokio::fs::read(&args.report).await?)?;
    let table = if args.allow_partial {
        ctrader::bake::render_finished(&report, &identity)?
    } else {
        ctrader::bake::render(&report, &identity)?
    };
    for row in &report.instruments {
        let metadata = data
            .forex_instrument_metadata(&row.measurement.symbol)
            .await?;
        ensure!(
            metadata.id == row.symbol_id
                && metadata.name == row.measurement.symbol
                && metadata.pip_size == row.measurement.pip_size,
            "broker symbol identity/pip metadata changed since calibration"
        );
    }
    ctrader::output::write(&args.out, &args.account, &table)?;
    tracing::info!(account=args.account,rows=report.instruments.len(),output=%args.out.display(),"baked verified cTrader paired spread profiles; broker trading is untouched");
    Ok(())
}
