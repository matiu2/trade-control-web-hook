use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use candle_cache::{CacheClient, CacheConfig};
use candle_model::Granularity;
use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use color_eyre::{
    Result,
    eyre::{Context, ensure, eyre},
};
use oanda_client::{OandaClient, account_candle_namespace, data_source::OandaDataSource};

use super::{Measurement, Report};
use crate::{
    Broker,
    cache::minutes_from_cache,
    compute::profile_from_minutes,
    universe::{WorkItem, work_items},
};

#[derive(Parser)]
pub struct Args {
    #[arg(long)]
    pub account: String,
    #[arg(long)]
    pub account_id: String,
    #[arg(long)]
    pub report: PathBuf,
    #[arg(long, default_value_t = 90)]
    pub days: i64,
    /// Spot checks remain unbakeable until the full broker catalogue is measured.
    #[arg(long)]
    pub only: Option<String>,
    /// Optional promotion after all 127 account instruments are measured.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

pub async fn run(args: Args) -> Result<()> {
    ensure!(
        args.days >= 90,
        "Live calibration requires at least 90 days"
    );
    let token = std::env::var("OANDA_TOKEN")
        .or_else(|_| std::env::var("OANDA_API_KEY"))
        .wrap_err("Set the live OANDA token")?;
    let client = OandaClient::new_live(token).with_candle_account(&args.account_id);
    let catalogue = client.get_instruments(&args.account_id).await?;
    ensure!(
        !catalogue.is_empty() && catalogue.iter().collect::<BTreeSet<_>>().len() == catalogue.len(),
        "Live account catalogue is empty or duplicated"
    );
    let items = catalogue_items(&catalogue, args.only.as_deref())?;
    ensure!(
        !items.is_empty(),
        "No requested live account instruments matched"
    );
    let mut report = load_report(&args, &catalogue, &items)?;
    report
        .instruments
        .retain(|row| reusable_measurement(row, &items, report.from_utc, report.to_utc));
    let config = CacheConfig {
        cache_dir: PathBuf::from(std::env::var("HOME")?)
            .join(".cache")
            .join(&report.cache_namespace),
        max_concurrent_broker_calls: 4,
        enable_background_eviction: false,
        ..CacheConfig::default()
    };
    let cache =
        Arc::new(CacheClient::new(config, OandaDataSource::new(client, &args.account_id)).await?);
    measure_remaining(cache, items, &mut report, &args.report).await?;
    report.complete =
        report.instruments.len() + report.failures.len() == report.requested_symbols.len();
    report.instruments.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    save_report(&args.report, &report)?;
    ensure!(
        report.failures.is_empty(),
        "{} live instruments failed; checkpoint retained for resume",
        report.failures.len()
    );
    if let Some(out) = args.out {
        std::fs::write(out, super::bake::render(&report)?)?;
    }
    tracing::info!(
        instruments = report.instruments.len(),
        "Live calibration completed"
    );
    Ok(())
}

fn reusable_measurement(
    row: &Measurement,
    items: &[WorkItem],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> bool {
    let metadata_matches = items.iter().any(|item| {
        item.symbol == row.symbol
            && item.spread_schedule == row.schedule
            && item.pip_size == row.pip_size
    });
    match super::bake::validate_measurement(row, from, to) {
        Ok(()) => metadata_matches,
        Err(error) => {
            tracing::info!(symbol=row.symbol,%error,"Rechecking checkpoint measurement");
            false
        }
    }
}

async fn measure_remaining(
    cache: Arc<CacheClient<OandaDataSource>>,
    items: Vec<WorkItem>,
    report: &mut Report,
    path: &Path,
) -> Result<()> {
    let remaining = items
        .into_iter()
        .filter(|item| {
            !report
                .instruments
                .iter()
                .any(|row| row.symbol == item.symbol)
        })
        .collect::<Vec<_>>();
    report.failures.clear();
    report.complete = false;
    save_report(path, report)?;
    tracing::info!(account=report.account,instruments=remaining.len(),from=%report.from_utc,to=%report.to_utc,namespace=report.cache_namespace,"Starting account-scoped live spread calibration");
    let permits = Arc::new(tokio::sync::Semaphore::new(4));
    let mut tasks = tokio::task::JoinSet::new();
    for item in remaining {
        let cache = cache.clone();
        let permits = permits.clone();
        let from = report.from_utc;
        let to = report.to_utc;
        tasks.spawn(async move {
            let symbol = item.symbol.clone();
            let result = async {
                let _permit = permits.acquire_owned().await?;
                measure(&cache, &item, from, to).await
            }
            .await;
            (symbol, result)
        });
    }
    while let Some(result) = tasks.join_next().await {
        let (symbol, result) = result?;
        match result {
            Ok(row) => {
                tracing::info!(symbol,bars=row.profile.n_bars,median_pips=row.profile.baseline_median_pips,review=?row.profile.review,completed=report.instruments.len()+1,total=report.requested_symbols.len(),"Live spread measured");
                report.instruments.push(row);
            }
            Err(error) => {
                tracing::warn!(symbol,%error,"Live spread measurement failed");
                report.failures.push((symbol, error.to_string()));
            }
        }
        save_report(path, report)?;
    }
    Ok(())
}

fn catalogue_items(catalogue: &[String], only: Option<&str>) -> Result<Vec<WorkItem>> {
    let assets = instrument_lookup::all().map_err(|error| eyre!("{error}"))?;
    let mut catalogued = BTreeMap::new();
    work_items(assets, &[Broker::Oanda], true)
        .into_iter()
        .for_each(|item| {
            catalogued.entry(item.symbol.clone()).or_insert(item);
        });
    let items = catalogue
        .iter()
        .map(|symbol| {
            if let Some(live) = instrument_lookup::oanda_live::get(symbol)? {
                let asset = instrument_lookup::by_id(&live.canonical)?
                    .ok_or_else(|| eyre!("Missing live canonical {}", live.canonical))?;
                return Ok(WorkItem {
                    broker: Broker::Oanda,
                    symbol: symbol.clone(),
                    class: live.asset_class,
                    display_name: live.display_name.clone(),
                    spread_schedule: asset.spread_schedule.clone(),
                    spread_schedule_tz: asset.spread_schedule_tz(),
                    pip_size: live.pip_size,
                });
            }
            if let Some(native) = instrument_lookup::by_broker_symbol_instrument(
                instrument_lookup::Broker::Oanda,
                symbol,
            )? && let Some(asset) = instrument_lookup::by_id(&native.canonical)?
            {
                return Ok(WorkItem {
                    broker: Broker::Oanda,
                    symbol: symbol.clone(),
                    class: native.class,
                    display_name: native.display_name.clone(),
                    spread_schedule: asset.spread_schedule.clone(),
                    spread_schedule_tz: asset.spread_schedule_tz(),
                    pip_size: native.pip_size,
                });
            }
            catalogued
                .get(symbol)
                .cloned()
                .ok_or_else(|| eyre!("Live instrument {symbol} is missing from instrument-lookup"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(items
        .into_iter()
        .filter(|item| only.is_none_or(|only| only.split(',').any(|symbol| symbol == item.symbol)))
        .collect())
}

#[cfg(test)]
mod tests;

fn load_report(args: &Args, catalogue: &[String], items: &[WorkItem]) -> Result<Report> {
    let requested = items
        .iter()
        .map(|item| item.symbol.clone())
        .collect::<BTreeSet<_>>();
    let namespace = account_candle_namespace(&args.account_id);
    if args.report.exists() {
        let report: Report = serde_json::from_slice(&std::fs::read(&args.report)?)?;
        ensure!(
            report.account == args.account
                && report.account_id == args.account_id
                && report.cache_namespace == namespace
                && report
                    .requested_symbols
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    == requested
                && report.catalogue_symbols.iter().collect::<BTreeSet<_>>()
                    == catalogue.iter().collect()
                && report.source == "oanda-live-account-candles-MBA",
            "Checkpoint identity or catalogue changed"
        );
        ensure!(
            report.to_utc <= Utc::now()
                && report.to_utc - report.from_utc == Duration::days(args.days),
            "Checkpoint calibration window changed"
        );
        let measured = report
            .instruments
            .iter()
            .map(|row| row.symbol.clone())
            .collect::<BTreeSet<_>>();
        ensure!(
            measured.len() == report.instruments.len() && measured.is_subset(&requested),
            "Checkpoint has duplicated or unrequested instruments"
        );
        return Ok(report);
    }
    let to = DateTime::from_timestamp(Utc::now().timestamp().div_euclid(86400) * 86400, 0)
        .ok_or_else(|| eyre!("Invalid calibration end time"))?;
    Ok(Report {
        account: args.account.clone(),
        account_id: args.account_id.clone(),
        source: "oanda-live-account-candles-MBA".into(),
        cache_namespace: namespace,
        from_utc: to - Duration::days(args.days),
        to_utc: to,
        requested_symbols: requested.into_iter().collect(),
        catalogue_symbols: catalogue.to_vec(),
        instruments: vec![],
        failures: vec![],
        complete: false,
    })
}

fn save_report(path: &Path, report: &Report) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(report)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

async fn measure(
    cache: &CacheClient<OandaDataSource>,
    item: &WorkItem,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Measurement> {
    let tz = item
        .spread_schedule_tz
        .as_deref()
        .ok_or_else(|| eyre!("No spread schedule for {}", item.symbol))?
        .parse()?;
    ensure!(
        item.pip_size.is_finite() && item.pip_size > 0.0,
        "Invalid pip size for {}",
        item.symbol
    );
    let candles = cache
        .get_candles_range_bid_ask(
            &item.symbol,
            from.fixed_offset(),
            to.fixed_offset(),
            Granularity::OneMinute,
        )
        .await?;
    let candles = candles
        .candles
        .into_iter()
        .filter(|candle| candle.timestamp >= from && candle.timestamp < to)
        .collect::<Vec<_>>();
    let minutes = minutes_from_cache(&candles, tz);
    let first = candles
        .first()
        .ok_or_else(|| eyre!("No closed live candles for {}", item.symbol))?
        .timestamp
        .with_timezone(&Utc);
    let last = candles
        .last()
        .ok_or_else(|| eyre!("No closed live candles for {}", item.symbol))?
        .timestamp
        .with_timezone(&Utc);
    let observed_utc_days = candles
        .iter()
        .map(|candle| candle.timestamp.with_timezone(&Utc).date_naive())
        .collect::<BTreeSet<_>>()
        .len();
    let measured = Measurement {
        symbol: item.symbol.clone(),
        schedule: item.spread_schedule.clone(),
        pip_size: item.pip_size,
        first_utc: first,
        last_utc: last,
        observed_utc_days,
        profile: profile_from_minutes(&minutes, item.pip_size),
    };
    super::bake::validate_measurement(&measured, from, to)?;
    Ok(measured)
}
