//! Complete, checkpointed TradeNation catalogue spread measurement.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use color_eyre::{
    Result,
    eyre::{WrapErr, ensure, eyre},
};
use serde::{Deserialize, Serialize};

use crate::{
    BaselineRow, Broker, ReviewStatus, SpreadProfile, cache::CachedSource, universe::WorkItem,
};

mod output;
use output::merge_tables;

#[derive(Parser)]
pub struct Args {
    #[arg(
        long,
        default_value = "spread-baseline-gen/validation/tradenation-60d.json"
    )]
    pub report: PathBuf,
    #[arg(long, default_value = "../candle-cache/src/spread_profiles/candle.rs")]
    pub out: PathBuf,
    /// Maximum instruments measured concurrently. Broker calls also use the shared cache semaphore.
    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,
    /// M1 retention is about 65 calendar days; keep the default inside it.
    #[arg(long, default_value_t = 60)]
    pub days: i64,
    /// Bound this invocation; subsequent runs resume the same fixed window.
    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Serialize, Deserialize)]
pub struct Report {
    source: String,
    from_utc: DateTime<Utc>,
    to_utc: DateTime<Utc>,
    catalogue: Vec<String>,
    measurements: BTreeMap<String, Measurement>,
    failures: BTreeMap<String, String>,
    complete: bool,
}

#[derive(Serialize, Deserialize)]
struct Measurement {
    schedule: String,
    pip_size: f64,
    first_utc: DateTime<Utc>,
    last_utc: DateTime<Utc>,
    observed_days: usize,
    profile: SpreadProfile,
}

pub fn universe() -> Result<Vec<WorkItem>> {
    let assets = instrument_lookup::tradenation::assets()?;
    Ok(crate::universe::work_items(
        assets,
        &[Broker::TradeNation],
        true,
    ))
}

pub async fn run(args: Args) -> Result<()> {
    ensure!(args.concurrency > 0, "concurrency must be positive");
    ensure!(
        args.days >= 45,
        "Use at least 45 calendar days for calibration"
    );
    let _report_lock = output::lock(&args.report)?;
    let items = universe()?;
    let mut report = load_report(&args.report, &items, args.days)?;
    report.complete = false;
    save_report(&args.report, &report)?;
    let remaining = items
        .into_iter()
        .filter(|item| !report.measurements.contains_key(&item.symbol))
        .take(args.limit.unwrap_or(usize::MAX))
        .collect::<Vec<_>>();
    tracing::info!(
        total = report.catalogue.len(),
        measured = report.measurements.len(),
        pending = remaining.len(),
        "TradeNation catalogue bake"
    );
    if !remaining.is_empty() {
        let source = Arc::new(CachedSource::open(Broker::TradeNation, None).await?);
        measure_remaining(&args, &source, &mut report, remaining).await?;
    }
    promote(&args.out, &report)?;
    report.complete =
        report.measurements.len() == report.catalogue.len() && report.failures.is_empty();
    save_report(&args.report, &report)?;
    tracing::info!(
        measured = report.measurements.len(),
        total = report.catalogue.len(),
        failures = report.failures.len(),
        complete = report.complete,
        "TradeNation spread coverage"
    );
    ensure!(
        report.complete,
        "TradeNation bake incomplete: {} of {} measured; {} failures. Resume with the same report.",
        report.measurements.len(),
        report.catalogue.len(),
        report.failures.len()
    );
    Ok(())
}

fn load_report(path: &Path, items: &[WorkItem], days: i64) -> Result<Report> {
    let catalogue = items
        .iter()
        .map(|item| item.symbol.clone())
        .collect::<Vec<_>>();
    let source = format!(
        "tradenation-{}-paired-M1",
        std::env::var("TN_ACCOUNT_TYPE")
            .unwrap_or_else(|_| "demo".into())
            .to_ascii_lowercase()
    );
    if path.exists() {
        let mut report: Report = serde_json::from_slice(&std::fs::read(path)?)?;
        validate_window(&report, Utc::now(), days)?;
        ensure!(
            report.source == source,
            "Checkpoint source changed; use a new report"
        );
        if report.catalogue != catalogue {
            tracing::info!(
                old = report.catalogue.len(),
                new = catalogue.len(),
                "Reconciling refreshed TradeNation catalogue"
            );
            report.catalogue = catalogue;
            report
                .failures
                .retain(|symbol, _| items.iter().any(|item| item.symbol == *symbol));
        }
        report.measurements.retain(|symbol, row| {
            items.iter().any(|item| {
                item.symbol == *symbol
                    && item.spread_schedule == row.schedule
                    && item.pip_size == row.pip_size
            }) && validate_measurement(row, report.from_utc, report.to_utc).is_ok()
        });
        return Ok(report);
    }
    let to_utc = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| eyre!("invalid UTC midnight"))?
        .and_utc();
    Ok(Report {
        source,
        from_utc: to_utc - Duration::days(days),
        to_utc,
        catalogue,
        measurements: BTreeMap::new(),
        failures: BTreeMap::new(),
        complete: false,
    })
}

async fn measure_remaining(
    args: &Args,
    source: &Arc<CachedSource>,
    report: &mut Report,
    items: Vec<WorkItem>,
) -> Result<()> {
    let permits = Arc::new(tokio::sync::Semaphore::new(args.concurrency));
    let mut tasks = tokio::task::JoinSet::new();
    for item in items {
        let source = source.clone();
        let permits = permits.clone();
        let (from, to) = (report.from_utc, report.to_utc);
        tasks.spawn(async move {
            let symbol = item.symbol.clone();
            let result = async {
                let _permit = permits.acquire_owned().await?;
                tokio::time::timeout(
                    std::time::Duration::from_secs(600),
                    measure(&source, &item, from, to),
                )
                .await
                .wrap_err("TradeNation instrument measurement timed out")?
            }
            .await;
            (symbol, result)
        });
    }
    while let Some(result) = tasks.join_next().await {
        let (symbol, result) = result?;
        match result {
            Ok(measurement) => {
                tracing::info!(
                    symbol,
                    bars = measurement.profile.n_bars,
                    days = measurement.observed_days,
                    "Measured TradeNation spread"
                );
                report.failures.remove(&symbol);
                report.measurements.insert(symbol, measurement);
            }
            Err(error) => {
                tracing::warn!(symbol, %error, "TradeNation remains uncalibrated");
                report.failures.insert(symbol, format!("{error:#}"));
            }
        }
        report.complete = false;
        save_report(&args.report, report)?;
    }
    Ok(())
}

async fn measure(
    source: &CachedSource,
    item: &WorkItem,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Measurement> {
    let tz = item
        .spread_schedule_tz
        .as_deref()
        .ok_or_else(|| eyre!("Missing spread clock"))?
        .parse::<chrono_tz::Tz>()?;
    let evidence = source.evidence(&item.symbol, from, to, tz).await?;
    let mut profile = crate::profile_from_minutes(&evidence.bars, item.pip_size);
    if item.class == instrument_lookup::AssetClass::Stock
        && profile.review == ReviewStatus::InsufficientData
    {
        profile = crate::compute::profile_from_sparse_minutes(&evidence.bars, item.pip_size);
    }
    let row = Measurement {
        schedule: item.spread_schedule.clone(),
        pip_size: item.pip_size,
        first_utc: evidence
            .first_utc
            .ok_or_else(|| eyre!("No paired candles"))?,
        last_utc: evidence
            .last_utc
            .ok_or_else(|| eyre!("No paired candles"))?,
        observed_days: evidence.observed_days,
        profile,
    };
    validate_measurement(&row, from, to)?;
    Ok(row)
}

fn validate_measurement(row: &Measurement, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<()> {
    ensure!(
        row.pip_size.is_finite() && row.pip_size > 0.0,
        "Invalid pip size"
    );
    ensure!(
        matches!(
            row.profile.review,
            ReviewStatus::Reviewed | ReviewStatus::MaskNotComputable
        ) && row.profile.baseline_median_pips.is_finite()
            && row.profile.baseline_median_pips > 0.0
            && row
                .profile
                .hour_p90_frac
                .iter()
                .all(|value| value.is_finite() && *value >= 0.0)
            && row.profile.hour_p90_frac.iter().any(|value| *value > 0.0),
        "No usable measured spread forecast"
    );
    ensure!(
        row.first_utc >= from
            && row.last_utc < to
            && row.first_utc <= row.last_utc - Duration::days(7)
            && row.last_utc >= to - Duration::days(7)
            && row.observed_days >= 10,
        "Incomplete history: {} observed days, {} through {}",
        row.observed_days,
        row.first_utc,
        row.last_utc
    );
    Ok(())
}

fn validate_window(report: &Report, now: DateTime<Utc>, days: i64) -> Result<()> {
    ensure!(
        report.to_utc <= now && report.to_utc - report.from_utc == Duration::days(days),
        "Invalid checkpoint window"
    );
    ensure!(
        now - report.to_utc <= Duration::days(7),
        "Checkpoint is older than seven days; use a new --report path for fresh calibration"
    );
    Ok(())
}

fn save_report(path: &Path, report: &Report) -> Result<()> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    output::atomic_write(path, &serde_json::to_string_pretty(report)?)
}

fn promote(path: &Path, report: &Report) -> Result<()> {
    validate_window(
        report,
        Utc::now(),
        (report.to_utc - report.from_utc).num_days(),
    )?;
    if report.measurements.is_empty() {
        return Ok(());
    }
    let rows = report
        .measurements
        .iter()
        .map(|(symbol, row)| {
            validate_measurement(row, report.from_utc, report.to_utc)?;
            Ok(BaselineRow {
                broker: Broker::TradeNation,
                symbol: symbol.clone(),
                display_name: symbol.clone(),
                spread_schedule: row.schedule.clone(),
                profile: row.profile.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let incoming = crate::render_table(&rows);
    let _output_lock = output::lock(path)?;
    let existing =
        std::fs::read_to_string(path).wrap_err("Read existing spread table before merging")?;
    output::atomic_write(path, &merge_tables(&existing, &incoming)?)
}

#[cfg(test)]
mod tests;
