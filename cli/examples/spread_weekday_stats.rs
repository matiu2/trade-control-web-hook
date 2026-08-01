//! **Does day-of-week carry spread information the hour-of-day forecast misses?**
//!
//! # Why ask
//!
//! `spread-baseline-gen` bakes a p90 spread forecast per **schedule-local hour**,
//! pooling all five weekdays into each hour's bucket. Two well-known mechanisms
//! would break that pooling if they are real:
//!
//! - **Friday NY close** — weekend risk-off, books thinning into the weekly
//!   close. A Friday-only blowout is diluted by four normal days, so the pooled
//!   p90 would *under*-size Friday and slightly over-size Mon–Thu.
//! - **Sunday/Monday open** — the reopen after the weekend gap, where spreads
//!   are notoriously wide.
//!
//! The forward-looking SL floor
//! (`max(10×last_candle, 10×expected_hour, desired)`) reads that forecast
//! directly, so any systematic weekday effect is a sizing error on every trade
//! resting into that hour.
//!
//! # The confound this must avoid
//!
//! Comparing "all Friday bars" against "all other bars" re-measures **hour mix**,
//! not weekday: the trading week starts and ends mid-session, so Friday and
//! Sunday carry a different distribution of hours than a midweek day. Pooling
//! across hours would score that scheduling artefact as a weekday effect.
//!
//! So every comparison here is **within an (instrument, local-hour) cell**: a
//! weekday's spread is scored only against the *same instrument at the same
//! local hour on other weekdays*. A cell needs samples from ≥3 distinct weekdays
//! to contribute, so a thin cell can't produce a large ratio from noise.
//!
//! # Metric
//!
//! `spread_frac = (ask − bid) / mid` — scale-free, the same quantity
//! `spread-baseline-gen` buckets, so a result here maps directly onto whether
//! the baked table should gain a weekday dimension.
//!
//! Reported as a **ratio to the cell's all-weekday median**, so 1.00 means "this
//! weekday is exactly typical for this instrument at this hour".
//!
//! # Reading the result
//!
//! A weekday dimension is worth building if some weekday's median ratio is
//! materially off 1.0 (say ≥1.15 or ≤0.87) **consistently across instruments**.
//! If every weekday sits near 1.0, hour-of-day already captures the structure and
//! adding a weekday axis would divide each bucket's sample count by ~5 for
//! nothing — strictly worse, since thinner buckets make the p90 noisier.
//!
//! Note this asks only whether the *forecast* should be finer. It says nothing
//! about signal validity — see `spread_hour_candle_stats.rs` for that question.
//!
//! ```sh
//! cargo run -p trade-control-cli --example spread_weekday_stats --release -- \
//!     --broker oanda --granularity h1 --max-instruments 60 --days 1460
//! ```

use std::collections::BTreeMap;

use candle_cache::{CacheClient, CacheConfig};
use candle_model::{BidAskCandle, Granularity};
use chrono::{DateTime, Datelike, Duration, FixedOffset, Utc, Weekday};
use oanda_client::{OandaClient, data_source::OandaDataSource};
use tradenation_api::TradeNationClient;

/// A cell must see this many distinct weekdays before it contributes, so a
/// sparsely-sampled (instrument, hour) can't manufacture a ratio from one day.
const MIN_WEEKDAYS_PER_CELL: usize = 3;
/// Minimum bars for one (instrument, hour, weekday) bucket to be usable.
const MIN_BARS_PER_BUCKET: usize = 8;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let granularity = arg(&args, "--granularity").unwrap_or_else(|| "h1".into());
    let max_instruments: usize = arg(&args, "--max-instruments")
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let days: i64 = arg(&args, "--days")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1460);
    let broker = arg(&args, "--broker").unwrap_or_else(|| "oanda".into());
    let json_out = arg(&args, "--json");
    if broker != "oanda" && broker != "tradenation" {
        return Err(format!("--broker must be oanda|tradenation, got {broker}").into());
    }
    let gran = match granularity.as_str() {
        "h1" => Granularity::OneHour,
        "m15" => Granularity::FifteenMinutes,
        other => return Err(format!("unsupported --granularity {other}").into()),
    };

    let instruments = universe(&broker, max_instruments);
    eprintln!(
        "weekday spread study: broker={broker}, {} instruments, {granularity}, {days}d lookback",
        instruments.len()
    );
    eprintln!(
        "  metric: spread_frac = (ask-bid)/mid, scored WITHIN each (instrument, local-hour) cell\n\
           so weekday effects can't be confounded by the week's hour mix\n"
    );

    let to = Utc::now();
    let from = to - Duration::days(days);

    // `CacheClient` is generic over its `DataSource`, so the two brokers yield
    // different concrete types and can't share one binding — each arm runs the
    // whole pass and hands back the buckets.
    let cells = if broker == "oanda" {
        let dir = cache_dir().join("candle_cache_oanda");
        let client =
            CacheClient::new(CacheConfig::default().with_cache_dir(dir), oanda_source()?).await?;
        collect(&client, &instruments, gran, from, to).await
    } else {
        let dir = cache_dir().join("candle_cache_tradenation");
        let client = CacheClient::new(
            CacheConfig::default().with_cache_dir(dir),
            TradeNationClient::new_demo(),
        )
        .await?;
        collect(&client, &instruments, gran, from, to).await
    };

    report(&cells, json_out.as_deref())
}

fn oanda_source() -> Result<OandaDataSource, Box<dyn std::error::Error>> {
    let token = std::env::var("OANDA_TOKEN")
        .map_err(|_| "OANDA_TOKEN not set (needed as the cache's miss-fallback)")?;
    let account_id = std::env::var("OANDA_ACCOUNT_ID")
        .map_err(|_| "OANDA_ACCOUNT_ID not set (needed as the cache's miss-fallback)")?;
    Ok(OandaDataSource::new(OandaClient::new(token), account_id))
}

/// One (instrument, local-hour, weekday) bucket's median spread fraction.
#[derive(Debug, Clone)]
struct Bucket {
    instrument: String,
    hour: u32,
    /// Days from Monday (0..=6) — `Weekday` isn't `Ord`, so buckets key on this.
    weekday: u32,
    median_spread_frac: f64,
    bars: usize,
}

async fn collect<D>(
    client: &CacheClient<D>,
    instruments: &[String],
    gran: Granularity,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<Bucket>
where
    D: candle_cache::BidAskDataSource,
{
    let mut out = Vec::new();
    for instrument in instruments {
        let series = match client
            .get_candles_range_bid_ask(instrument, fx(from), fx(to), gran)
            .await
        {
            Ok(s) => s,
            Err(err) => {
                eprintln!("  {instrument}: fetch failed ({err}); skipping");
                continue;
            }
        };
        // (hour, weekday) -> spread fractions
        let mut raw: BTreeMap<(u32, u32), Vec<f64>> = BTreeMap::new();
        for c in series.candles.iter() {
            let (bid, ask) = (c.bid_close(), c.ask_close());
            let mid = (bid + ask) / 2.0;
            let spread = ask - bid;
            if !(mid.is_finite() && spread.is_finite() && mid > 0.0 && spread > 0.0) {
                continue;
            }
            let t = c.timestamp;
            raw.entry((t.hour_utc(), t.weekday_utc().num_days_from_monday()))
                .or_default()
                .push(spread / mid);
        }
        for ((hour, weekday_num), mut v) in raw {
            if v.len() < MIN_BARS_PER_BUCKET {
                continue;
            }
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            out.push(Bucket {
                instrument: instrument.clone(),
                hour,
                weekday: weekday_num,
                median_spread_frac: v[v.len() / 2],
                bars: v.len(),
            });
        }
        eprint!(".");
    }
    eprintln!();
    out
}

/// Score every bucket against its own (instrument, hour) cell median, then
/// aggregate per weekday.
fn report(cells: &[Bucket], json_out: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    // Group by (instrument, hour) — the cell within which weekdays are compared.
    let mut by_cell: BTreeMap<(String, u32), Vec<&Bucket>> = BTreeMap::new();
    for b in cells {
        by_cell
            .entry((b.instrument.clone(), b.hour))
            .or_default()
            .push(b);
    }

    let mut ratios: BTreeMap<u32, Vec<f64>> = BTreeMap::new();
    let mut used_cells = 0usize;
    for buckets in by_cell.values() {
        if buckets.len() < MIN_WEEKDAYS_PER_CELL {
            continue;
        }
        let mut meds: Vec<f64> = buckets.iter().map(|b| b.median_spread_frac).collect();
        meds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let cell_median = meds[meds.len() / 2];
        if !(cell_median.is_finite() && cell_median > 0.0) {
            continue;
        }
        used_cells += 1;
        for b in buckets {
            ratios
                .entry(b.weekday)
                .or_default()
                .push(b.median_spread_frac / cell_median);
        }
    }

    if used_cells == 0 {
        println!("no usable (instrument, hour) cells — nothing to report");
        return Ok(());
    }

    let total_bars: usize = cells.iter().map(|b| b.bars).sum();
    println!("\n=== weekday spread ratio, within (instrument, local-hour) cells ===");
    println!(
        "{used_cells} cells, {} buckets, {total_bars} bars — ratio 1.00 = typical for that \
         instrument+hour\n",
        cells.len(),
    );
    println!("{:<10} {:>8} {:>8} {:>8} {:>9}", "weekday", "median", "p25", "p75", "buckets");

    let order: [(u32, &str); 7] = [
        (0, "Mon"),
        (1, "Tue"),
        (2, "Wed"),
        (3, "Thu"),
        (4, "Fri"),
        (5, "Sat"),
        (6, "Sun"),
    ];
    let mut rows = Vec::new();
    for (wd_num, wd) in order {
        let Some(v) = ratios.get(&wd_num) else { continue };
        let mut v = v.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let q = |f: f64| v[((v.len() as f64 - 1.0) * f).round() as usize];
        let (med, p25, p75) = (q(0.5), q(0.25), q(0.75));
        println!("{wd:<10} {med:>8.3} {p25:>8.3} {p75:>8.3} {:>9}", v.len());
        rows.push((wd.to_string(), med, p25, p75, v.len()));
    }

    // The verdict, stated rather than left to the reader.
    let worst = rows
        .iter()
        .map(|(w, m, ..)| (w.clone(), (*m - 1.0).abs(), *m))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    println!();
    match worst {
        Some((wd, dev, med)) if dev >= 0.15 => println!(
            "VERDICT: {wd} deviates {:.0}% from typical (median ratio {med:.2}) — a weekday \
             dimension looks WORTH building.",
            dev * 100.0
        ),
        Some((wd, dev, med)) => println!(
            "VERDICT: largest deviation is {wd} at {:.0}% (median ratio {med:.2}), under the 15% \
             bar. Hour-of-day already captures the structure; adding a weekday axis would divide \
             each bucket's samples by ~5 for no gain (thinner buckets ⇒ noisier p90).",
            dev * 100.0
        ),
        None => println!("VERDICT: no weekday rows — insufficient data."),
    }

    if let Some(path) = json_out {
        let json = format!(
            "{{\"cells\":{used_cells},\"weekdays\":[{}]}}",
            rows.iter()
                .map(|(w, m, p25, p75, n)| format!(
                    "{{\"weekday\":\"{w}\",\"median\":{m},\"p25\":{p25},\"p75\":{p75},\"buckets\":{n}}}"
                ))
                .collect::<Vec<_>>()
                .join(",")
        );
        std::fs::write(path, json)?;
        eprintln!("wrote {path}");
    }
    Ok(())
}

/// UTC hour / weekday helpers. Deliberately UTC here (not schedule-local): this
/// probe asks whether a weekday axis adds anything *given* the existing hour
/// bucketing, and mixing in a second timezone transform would confound the two.
/// The generator's local-hour bucketing already handles DST; if a weekday effect
/// shows up here it would be re-measured in local hours before being baked.
trait TimeParts {
    fn hour_utc(&self) -> u32;
    fn weekday_utc(&self) -> Weekday;
}
impl TimeParts for DateTime<FixedOffset> {
    fn hour_utc(&self) -> u32 {
        use chrono::Timelike;
        self.with_timezone(&Utc).hour()
    }
    fn weekday_utc(&self) -> Weekday {
        self.with_timezone(&Utc).weekday()
    }
}

/// Every instrument this broker has a baked row for. Unlike the rubbish-candle
/// probe we do NOT filter to flagged masks: the question is whether weekday
/// matters at *any* hour, so restricting to elevated-hour instruments would beg
/// it. Parses the generated table directly (it lives behind a private module).
fn universe(broker: &str, max: usize) -> Vec<String> {
    const TABLE: &str = include_str!("../../core/src/spread_baseline_candle.rs");
    let prefix = format!("(\"{broker}\"");
    let mut out = Vec::new();
    for line in TABLE.lines() {
        let line = line.trim();
        if !line.starts_with(&prefix) {
            continue;
        }
        let fields: Vec<&str> = line.trim_start_matches('(').split(',').collect();
        let Some(symbol) = fields.get(1) else { continue };
        out.push(symbol.trim().trim_matches('"').to_string());
        if out.len() >= max {
            break;
        }
    }
    out
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn fx(t: DateTime<Utc>) -> DateTime<FixedOffset> {
    t.into()
}

fn cache_dir() -> std::path::PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".cache")
        })
}
