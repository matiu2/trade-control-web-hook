//! Bounded, sequential, read-only paired history calibration.
use super::Report;
use broker_ctrader::{access::Profile, market_data::ChartData};
use chrono::{DateTime, Duration, Utc};
use color_eyre::{Result, eyre::ensure};
mod args;
mod full_universe;
mod identity;
mod measure;
mod universe;
mod workload;
pub use args::Args;
pub use full_universe::verify_full_universe;
pub use identity::{demo_config, identity};
use measure::measure;
pub async fn run(args: Args) -> Result<()> {
    ensure!(
        (1..=24).contains(&args.window_hours) && (1..=366).contains(&args.days),
        "invalid calibration window parameters"
    );
    ensure!(
        args.out.extension().is_some_and(|e| e == "json"),
        "--out must be a JSON report path"
    );
    let destination = super::checkpoint::Destination::acquire(&args.out, args.resume.as_deref())?;
    let previous = destination.read_resume().await?;
    let profile = Profile::load(&args.account)?;
    demo_config(&profile)?;
    let data = ChartData::connect_profile(&args.account, profile).await?;
    let identity = identity(&args.account, &data).await?;
    let only = previous
        .as_ref()
        .map(|r| r.requested_symbols.as_slice())
        .unwrap_or(&args.only);
    let native_stocks = broker_ctrader::catalogue::stock_symbols()?;
    let stocks = args.stocks
        || previous.as_ref().is_some_and(|report| {
            report
                .requested_symbols
                .iter()
                .any(|name| native_stocks.contains(&name.as_str()))
        });
    let (requested, excluded) = universe::select(&data, only, args.all_instruments, stocks).await?;
    let end = closed_minute(args.to.unwrap_or_else(Utc::now));
    let start = args
        .from
        .unwrap_or(end - Duration::days(i64::from(args.days)));
    ensure!(
        start < end && end <= Utc::now() && start == closed_minute(start),
        "invalid closed-minute UTC sampling window"
    );
    ensure!(
        end - start <= Duration::days(366),
        "sampling window cannot exceed 366 days"
    );
    let mut report = if let Some(mut report) = previous {
        super::checkpoint::validate(&report, &identity, &requested)?;
        report.complete = false;
        report.failures.clear();
        report
    } else {
        Report {
            schema_version: 1,
            identity,
            source: "paired-ticks-m1".into(),
            from_utc: start,
            to_utc: end,
            requested_symbols: requested,
            complete: false,
            excluded,
            instruments: vec![],
            failures: vec![],
        }
    };
    let remaining = super::checkpoint::remaining(&report);
    let workload = workload::estimate(report.from_utc, report.to_utc, args.window_hours);
    tracing::info!(account=args.account, instruments=remaining.len(), from=%report.from_utc, to=%report.to_utc, window_hours=args.window_hours, windows_per_symbol=workload.windows, nominal_unpaginated_history_requests=workload.requests * remaining.len() as i64, "calibration plan: sequential bounded windows, native M1 plus two quote seeds and two historical quote sides; pagination/seed search increase calls, closed sessions and warm cache reduce calls");
    if args.plan {
        return Ok(());
    }
    destination.save(&report).await?;
    let feed = data.cached_feed().await?;
    for symbol in remaining {
        let measured = measure(
            &data,
            &feed,
            &symbol,
            report.from_utc,
            report.to_utc,
            args.window_hours,
        )
        .await;
        match measured {
            Ok(row) => {
                tracing::info!(symbol, bars=row.measurement.profile.n_bars, median_pips=row.measurement.profile.baseline_median_pips, review=?row.measurement.profile.review, "paired spread measurement complete");
                report.instruments.push(row);
            }
            Err(error) => {
                tracing::warn!(symbol,%error,"cTrader pair remains uncovered; no estimated spread emitted");
                report.failures.push(crate::mt5::Failure {
                    symbol,
                    error: format!("{error:#}"),
                });
            }
        }
        destination.save(&report).await?;
        ensure!(
            !data.is_closed(),
            "cTrader session closed; resume this checkpoint"
        );
    }
    report.complete = report.failures.is_empty();
    destination.save(&report).await?;
    ensure!(
        report.complete,
        "calibration has {} failed instruments; details in {}",
        report.failures.len(),
        args.out.display()
    );
    Ok(())
}

fn closed_minute(time: DateTime<Utc>) -> DateTime<Utc> {
    time - Duration::seconds(time.timestamp().rem_euclid(60))
        - Duration::nanoseconds(i64::from(time.timestamp_subsec_nanos()))
}
