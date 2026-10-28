//! Read-only diagnostics for broker histories which fail spread calibration.
use color_eyre::Result;
use spread_baseline_gen::{Broker, cache::CachedSource};
use std::sync::Arc;
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
    let source = Arc::new(CachedSource::open(Broker::TradeNation, None).await?);
    let to = chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| color_eyre::eyre::eyre!("invalid midnight"))?
        .and_utc();
    let mut tasks = tokio::task::JoinSet::new();
    for symbol in std::env::args().skip(1) {
        let source = source.clone();
        tasks.spawn(async move {
            let evidence = source.evidence(&symbol, to - chrono::Duration::days(60), to, chrono_tz::Europe::London).await?;
            let profile = spread_baseline_gen::profile_from_minutes(&evidence.bars, 0.01);
            let min_mid = evidence.bars.iter().map(|bar| bar.mid_close).fold(f64::INFINITY, f64::min);
            let max_mid = evidence.bars.iter().map(|bar| bar.mid_close).fold(f64::NEG_INFINITY, f64::max);
            let positive_spreads = evidence.bars.iter().filter(|bar| bar.spread_frac > 0.0).count();
            tracing::info!(target: "spread_baseline_gen", symbol, bars=evidence.bars.len(), days=evidence.observed_days, first=?evidence.first_utc, last=?evidence.last_utc, min_mid, max_mid, positive_spreads, review=?profile.review, vol=profile.vol, "TradeNation history diagnosis");
            Ok::<_, color_eyre::Report>(())
        });
    }
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    Ok(())
}
