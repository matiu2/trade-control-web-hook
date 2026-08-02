//! **Which city's clock is an instrument's spread spike pegged to?** — decide by
//! data, not by intuition about the currency pair.
//!
//! # The question
//!
//! `AUD/NZD` is catalogued with `spread_schedule = "ny"`, which looks wrong at a
//! glance: neither leg is a US currency, so Sydney seems the natural anchor. But
//! the schedule does not name "the market that trades this pair" — it names **the
//! clock the spike is pegged to**, and those are different claims. Getting it
//! wrong is not cosmetic: the schedule chooses the timezone the mask and forecast
//! are bucketed in, so a mis-set schedule smears the spike across two local hours
//! whenever that zone's DST disagrees with the true one.
//!
//! # How to decide it
//!
//! A spike pegged to city X is *sharp* when bucketed in X's local time and
//! *smeared* when bucketed in the wrong zone — because the two zones' DST rules
//! disagree for part of the year, so a fixed instant lands in different local
//! hours on different dates. (NY and Sydney are the interesting pair: their DST
//! seasons are opposite, so they disagree for months at a time.)
//!
//! So for each candidate timezone this bakes the same minute series and reports
//! **concentration**: what fraction of the whole window's total excess spread
//! falls in that zone's single worst local hour. The right zone maximises it.
//!
//! A flat result across zones means the instrument has no clock-pegged spike at
//! all and the schedule is irrelevant for it.
//!
//! Run: `cargo run -p spread-baseline-gen --example which_schedule -- \
//!       --instrument AUD_NZD --days 90`

use clap::Parser;
use color_eyre::eyre::Result;
use spread_baseline_gen::compute::MinuteBar;

#[derive(Parser, Debug)]
#[command(about = "Test which city's clock an instrument's spread spike is pegged to")]
struct Args {
    /// OANDA instrument symbol, e.g. `AUD_NZD`.
    #[arg(long, default_value = "AUD_NZD")]
    instrument: String,

    /// Lookback window in days. Must span a DST transition in at least one
    /// candidate zone for the comparison to have any power — 90d+ is safe.
    #[arg(long, default_value_t = 90)]
    days: i64,

    /// Candidate timezones to score, comma-separated IANA names.
    #[arg(
        long,
        default_value = "America/New_York,Australia/Sydney,Europe/London,Pacific/Auckland,UTC"
    )]
    zones: String,
}

/// Re-stamp a minute series into a different timezone's local hour.
///
/// The fetch already stamped `local_hour` for ONE zone, so re-deriving for
/// another needs the original instant — which `utc_minute_of_day` alone cannot
/// give (it has no date, so it cannot know that zone's DST state on that day).
/// The caller therefore keeps the fetched UTC timestamps alongside.
fn rebucket(
    bars: &[(chrono::DateTime<chrono::Utc>, MinuteBar)],
    tz: chrono_tz::Tz,
) -> [Vec<f64>; 24] {
    use chrono::Timelike;
    let mut by_hour: [Vec<f64>; 24] = Default::default();
    for (ts, b) in bars {
        let h = ts.with_timezone(&tz).hour() as usize;
        if h < 24 {
            by_hour[h].push(b.spread_frac);
        }
    }
    by_hour
}

/// What fraction of the window's total EXCESS spread lands in the single worst
/// local hour of this bucketing.
///
/// Excess, not raw: every hour carries the instrument's baseline spread, and
/// including it would dilute the measure identically for every zone. Subtracting
/// the median-hour level leaves only the spike, which is the thing being located.
/// Returns `(worst_hour, concentration)`.
fn concentration(by_hour: &[Vec<f64>; 24]) -> (usize, f64) {
    let totals: Vec<f64> = by_hour.iter().map(|v| v.iter().sum::<f64>()).collect();
    let counts: Vec<usize> = by_hour.iter().map(|v| v.len()).collect();

    // Baseline = median of the per-hour MEAN spread, so a fat hour cannot set it.
    let mut means: Vec<f64> = (0..24)
        .filter(|&h| counts[h] > 0)
        .map(|h| totals[h] / counts[h] as f64)
        .collect();
    if means.is_empty() {
        return (0, 0.0);
    }
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let baseline = means[means.len() / 2];

    let excess: Vec<f64> = (0..24)
        .map(|h| (totals[h] - baseline * counts[h] as f64).max(0.0))
        .collect();
    let total_excess: f64 = excess.iter().sum();
    if total_excess <= 0.0 {
        return (0, 0.0);
    }
    let worst = (0..24).max_by(|&a, &b| {
        excess[a]
            .partial_cmp(&excess[b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let worst = worst.unwrap_or(0);
    (worst, excess[worst] / total_excess)
}

/// Fetch M1 candles paired with their true UTC instants.
///
/// Mirrors [`spread_baseline_gen::fetch::fetch_oanda_minutes`]'s paging, but
/// keeps each candle's timestamp instead of reducing it to a minute-of-day —
/// the date is what selects a zone's DST state, so it cannot be discarded here.
async fn fetch_stamped(
    client: &oanda_client::OandaClient,
    instrument: &str,
    days: i64,
) -> Result<Vec<(chrono::DateTime<chrono::Utc>, MinuteBar)>> {
    use chrono::{Duration, Utc};
    use oanda_client::candles::Granularity;

    let now = Utc::now();
    let mut cursor = (now - Duration::days(days)).fixed_offset();
    let per = 5000usize;
    let mut out = Vec::new();
    let mut last_seen: Option<chrono::DateTime<Utc>> = None;
    loop {
        let resp = client
            .get_candles_from(instrument, cursor, per, Granularity::OneMinute)
            .await
            .map_err(|e| color_eyre::eyre::eyre!("get_candles_from({instrument}): {e}"))?;
        if resp.candles.is_empty() {
            break;
        }
        let mut latest = cursor.with_timezone(&Utc);
        for c in &resp.candles {
            let (Some(bid), Some(ask), Some(mid)) = (&c.raw.bid, &c.raw.ask, &c.raw.mid) else {
                continue;
            };
            let t = c.raw.time.with_timezone(&Utc);
            latest = latest.max(t);
            if t >= now || last_seen.is_some_and(|ls| t <= ls) {
                continue;
            }
            let (m, b, a) = (mid.c(), bid.c(), ask.c());
            if !(m.is_finite() && m > 0.0) {
                continue;
            }
            let spread = a - b;
            if !spread.is_finite() || spread < 0.0 {
                continue;
            }
            out.push((
                t,
                MinuteBar {
                    utc_minute_of_day: 0, // unused here; bucketing re-derives from `t`
                    local_hour: 0,
                    spread_frac: spread / m,
                    mid_close: m,
                },
            ));
            last_seen = Some(t);
        }
        if latest >= now || resp.candles.len() < per {
            break;
        }
        let next = latest + Duration::minutes(1);
        if next.fixed_offset() <= cursor {
            break;
        }
        cursor = next.fixed_offset();
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args = Args::parse();

    let token = std::env::var("OANDA_TOKEN")
        .or_else(|_| std::env::var("OANDA_API_KEY"))
        .map_err(|_| color_eyre::eyre::eyre!("set OANDA_TOKEN (or OANDA_API_KEY)"))?;
    let client = oanda_client::OandaClient::new(token);

    // Fetch once with real instants; every candidate zone re-buckets the same data.
    eprintln!(
        "fetching {} M1 candles, last {} days...",
        args.instrument, args.days
    );

    // Re-fetch stamped with real instants. Reconstructing dates by counting
    // minute-of-day wraps is WRONG across a weekend: the series jumps Friday
    // 21:00 → Sunday 21:00 with no wrap, so every subsequent bar would be dated
    // two days early and land in the wrong DST season — precisely the thing
    // under test. `MinuteBar` carries no date, so take the timestamps from the
    // raw candles instead.
    let stamped = fetch_stamped(&client, &args.instrument, args.days).await?;
    eprintln!("got {} minute bars\n", stamped.len());

    println!(
        "{} — where does the spread spike concentrate?\n",
        args.instrument
    );
    println!(
        "{:<22} {:>11} {:>16}",
        "timezone", "worst hour", "concentration"
    );
    let mut scored: Vec<(String, usize, f64)> = Vec::new();
    for name in args.zones.split(',') {
        let Ok(tz) = name.trim().parse::<chrono_tz::Tz>() else {
            eprintln!("skipping unparseable zone {name}");
            continue;
        };
        let (hour, conc) = concentration(&rebucket(&stamped, tz));
        println!("{:<22} {hour:>11} {:>15.1}%", name.trim(), conc * 100.0);
        scored.push((name.trim().to_string(), hour, conc));
    }

    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    if let Some((best, hour, conc)) = scored.first() {
        println!(
            "\nsharpest: {best} at local hour {hour} ({:.1}%)",
            conc * 100.0
        );
        if let Some((next, _, c2)) = scored.get(1) {
            println!(
                "runner-up: {next} ({:.1}%) — margin {:.1} points",
                c2 * 100.0,
                (conc - c2) * 100.0
            );
        }
    }
    Ok(())
}
