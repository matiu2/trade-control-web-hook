use std::path::{Path, PathBuf};

use candle_model::{BidAskDataSource, Granularity};
use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use color_eyre::{
    Result,
    eyre::{Context, ensure},
};
use mt5_data_source::{account_config::AccountConfig, tick_source::TickDataSource};
use spread_baseline_gen::mt5::{Failure, InstrumentReport, Report, instrument_report};

#[derive(Parser)]
#[command(
    about = "Measure account-specific MT5 spreads from validated cached M1 tick candles (read-only)"
)]
pub struct Args {
    #[arg(long)]
    account: String,
    #[arg(long, default_value_t = 90, value_parser = clap::value_parser!(u32).range(1..=366))]
    days: u32,
    /// Optional comma-separated native symbols; default is the account's entire live catalogue.
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// Reproducible UTC end, aligned to a closed minute. Defaults to now rounded down.
    #[arg(long)]
    to: Option<DateTime<Utc>>,
    /// JSON report path; the existing live Rust table cannot be overwritten.
    #[arg(long)]
    out: PathBuf,
    /// Preserve completed profiles and retry missing/failed symbols at the original dates.
    #[arg(long, conflicts_with_all = ["only", "to"])]
    resume: Option<PathBuf>,
}

pub async fn run(args: Args) -> Result<()> {
    ensure!(
        args.out.extension().is_some_and(|e| e == "json"),
        "--out must be a JSON report path"
    );
    let config = AccountConfig::load(&args.account)?;
    let trader = config.trader().await?;
    let symbols: Vec<String> = trader
        .symbols()
        .await?
        .into_iter()
        .map(|s| s.name)
        .collect();
    ensure!(
        args.only.iter().all(|s| symbols.contains(s)),
        "--only contains a symbol absent from this MT5 account"
    );
    let requested: Vec<String> = symbols
        .into_iter()
        .filter(|s| args.only.is_empty() || args.only.contains(s))
        .collect();
    ensure!(!requested.is_empty(), "account returned no instruments");
    let end = args.to.unwrap_or_else(Utc::now);
    ensure!(end <= Utc::now(), "calibration end is in the future");
    let end = end
        - Duration::seconds(end.timestamp().rem_euclid(60))
        - Duration::nanoseconds(i64::from(end.timestamp_subsec_nanos()));
    let mut report = Report {
        schema_version: 1,
        feed: format!("mt5-{}", args.account),
        login: config.login,
        server: config.server.clone(),
        from_utc: end - Duration::days(i64::from(args.days)),
        to_utc: end,
        requested_symbols: requested.clone(),
        complete: false,
        instruments: Vec::new(),
        failures: Vec::new(),
    };
    if let Some(path) = &args.resume {
        report = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        super::checkpoint::validate(&report, &args.account, &config, &requested, args.days)?;
        report.complete = false;
        report.failures.clear();
    }
    let requested = super::checkpoint::remaining(&report);
    let end = report.to_utc;
    save(&args.out, &report).await?;
    let desktop = config.desktop()?;
    desktop.identity().await.wrap_err("MT5 calibration bridge preflight")?;
    let source = config.candles().await?;
    for (index, symbol) in requested.iter().enumerate() {
        // A dead/unresponsive bridge is a run-wide interruption, not evidence
        // that every later instrument is unavailable. Leave them unattempted.
        desktop.identity().await.wrap_err("MT5 calibration bridge interrupted; resume this checkpoint")?;
        tracing::info!(
            symbol,
            instrument = index + 1,
            total = requested.len(),
            days = args.days,
            "MT5 spread calibration"
        );
        match measure(&source, symbol, report.from_utc, end).await {
            Ok(row) => {
                tracing::info!(symbol, median_pips=row.profile.baseline_median_pips,
                    p90_pips=row.profile.baseline_high_pips, hours=?row.elevated_local_hours,
                    review=?row.profile.review, "MT5 spread profile measured");
                report.instruments.push(row);
            }
            Err(error) => {
                desktop.identity().await.wrap_err("MT5 bridge interrupted; resume this checkpoint")?;
                tracing::warn!(symbol, %error, "MT5 calibration failed; no replacement profile emitted");
                report.failures.push(Failure {
                    symbol: symbol.clone(),
                    error: format!("{error:#}"),
                });
            }
        }
        save(&args.out, &report).await?;
    }
    report.complete = report.failures.is_empty();
    save(&args.out, &report).await?;
    ensure!(
        report.complete,
        "MT5 calibration incomplete: {} instrument(s) failed; see {}",
        report.failures.len(),
        args.out.display()
    );
    Ok(())
}

async fn measure(
    source: &TickDataSource,
    symbol: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<InstrumentReport> {
    let asset = spread_baseline_gen::mt5::metadata::resolve(symbol)?;
    let timezone = asset
        .spread_schedule_tz()
        .ok_or_else(|| eyre!("no spread schedule timezone for {symbol}"))?
        .parse()?;
    let mut candles = Vec::new();
    let mut cursor = from;
    while cursor < to {
        // Two-day native requests include trading bars across a weekend edge,
        // allowing MT5 to synchronize a newly selected instrument's series.
        let end = (cursor + Duration::days(2)).min(to);
        let chunk = fetch_window(source, symbol, cursor, end)
            .await
            .wrap_err_with(|| format!("MT5 {symbol} [{cursor}, {end})"))?;
        candles.extend(chunk.candles);
        cursor = end;
        tracing::info!(symbol, through=%cursor, minutes=candles.len(), "MT5 calibration window complete");
    }
    instrument_report(
        symbol,
        asset.pip_size,
        &asset.spread_schedule,
        timezone,
        &candles,
    )
}

async fn fetch_window(
    source: &TickDataSource,
    symbol: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<candle_model::BidAskCandles> {
    let fetch = || {
        source.get_candles_range_bid_ask(
            symbol,
            from.fixed_offset(),
            to.fixed_offset(),
            Granularity::OneMinute,
        )
    };
    match fetch().await {
        Err(error) if format!("{error:#}").contains("not synchronized") => {
            tracing::info!(
                symbol,
                "MT5 history not synchronized; retrying this window once"
            );
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            fetch().await
        }
        result => result,
    }
}

async fn save(path: &Path, report: &Report) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    tokio::fs::write(&temporary, serde_json::to_vec_pretty(report)?).await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    fn logging() {
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::fmt::layer())
            .try_init()
            .ok();
    }

    #[test]
    fn full_account_is_the_default_and_window_is_bounded() {
        logging();
        let a = Args::try_parse_from([
            "generate-mt5",
            "--account",
            "the5ers-competition",
            "--out",
            "/tmp/spreads.json",
        ])
        .unwrap();
        assert!(a.only.is_empty());
        assert_eq!(a.days, 90);
        assert!(
            Args::try_parse_from([
                "generate-mt5",
                "--account",
                "test",
                "--days",
                "0",
                "--out",
                "/tmp/spreads.json"
            ])
            .is_err()
        );
    }

    #[test]
    fn explicit_symbols_and_end_are_reproducible() {
        logging();
        let a = Args::try_parse_from([
            "generate-mt5",
            "--account",
            "test",
            "--only",
            "AUDUSD,EURCAD",
            "--to",
            "2026-10-10T00:00:00Z",
            "--out",
            "/tmp/spreads.json",
        ])
        .unwrap();
        assert_eq!(a.only, ["AUDUSD", "EURCAD"]);
        assert_eq!(a.to.unwrap().to_rfc3339(), "2026-10-10T00:00:00+00:00");
    }
}
