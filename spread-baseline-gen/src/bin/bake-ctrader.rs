//! Verify a completed paired report against the current demo identity, then bake.
use broker_ctrader::{access::Profile, market_data::ChartData};
use clap::Parser;
use color_eyre::Result;
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
    /// Add new same-account/window profiles without rewriting or deleting existing rows.
    #[arg(long, conflicts_with = "allow_partial")]
    merge: bool,
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
        let metadata = ctrader::calibration::metadata(&data, &row.measurement.symbol, true).await?;
        ctrader::bake::verify_broker_metadata(row, metadata.id, &metadata.name, metadata.pip_size)?;
    }
    if args.merge {
        ctrader::output::merge(&args.out, &args.account, &table)?;
    } else {
        ctrader::output::write(&args.out, &args.account, &table)?;
    }
    tracing::info!(account=args.account,rows=report.instruments.len(),append_only=args.merge,output=%args.out.display(),"baked verified cTrader paired spread profiles; broker trading is untouched");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn additive_promotion_is_explicit_and_cannot_promote_partial_reports() {
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer())
            .try_init()
            .ok();
        let base = ["bake-ctrader", "--account", "demo", "--report", "xpd.json"];
        let args = Args::try_parse_from(base.into_iter().chain(["--merge"])).unwrap();
        assert!(args.merge);
        assert!(!args.allow_partial);
        assert!(
            Args::try_parse_from(base.into_iter().chain(["--merge", "--allow-partial"])).is_err()
        );
    }
}
