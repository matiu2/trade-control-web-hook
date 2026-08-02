//! **Is the baked per-hour spread forecast right?** — an audit of the number
//! the SL floor sizes against.
//!
//! # The discrepancy under test
//!
//! The baked forecast for OANDA `AUD_NZD` reads ~2.5p at every ordinary hour.
//! Measuring the spread of 529 real H1 **closes** gave p90 = 1.8p. At the SL
//! floor's 10× multiple that gap is ~7 pips added to every stop, so it is worth
//! knowing which number is wrong — or whether they are simply different
//! statistics that were never comparable.
//!
//! # What this measures
//!
//! The generator's forecast is `p90` over every **M1 minute** in the hour
//! ([`compute::WIDEN_PERCENTILE`], via [`profile_from_minutes`]). An H1-close
//! sample takes **one** reading per hour: the spread at the moment the hour
//! ended. Those are different populations, and the minute population contains
//! the intra-hour excursions the close cannot see.
//!
//! So for each hour this prints, from the SAME minute series:
//!
//! - `p50/p75/p90/p99` over all minutes — the generator's population.
//! - `p90` over **hour-closing minutes only** (`:59`) — reconstructs the
//!   H1-close statistic from identical data, so the two are finally comparable.
//!
//! If the close-only column reproduces ~1.8p while the all-minutes column reads
//! ~2.5p, nothing is broken: the forecast is a **correct p90 of a wider
//! population**, and the open question becomes which population the SL floor
//! ought to size against. If both columns read ~2.5p, the earlier H1 measurement
//! was wrong. If both read ~1.8p, the bake is stale or came from another feed.
//!
//! Run: `cargo run -p spread-baseline-gen --example forecast_vs_reality -- \
//!       --instrument AUD_NZD --days 90`

use clap::Parser;
use color_eyre::eyre::Result;
use spread_baseline_gen::compute::MinuteBar;

#[derive(Parser, Debug)]
#[command(about = "Compare the baked spread forecast against freshly measured candles")]
struct Args {
    /// OANDA instrument symbol, e.g. `AUD_NZD`.
    #[arg(long, default_value = "AUD_NZD")]
    instrument: String,

    /// Lookback window in days — match the generator's `--days` to compare
    /// like with like (its default is 90).
    #[arg(long, default_value_t = 90)]
    days: i64,

    /// Schedule timezone to bucket local hours in, matching the baked row's
    /// `schedule` column (`ny` ⇒ America/New_York).
    #[arg(long, default_value = "America/New_York")]
    tz: String,

    /// Pip size for the pips columns.
    #[arg(long, default_value_t = 0.0001)]
    pip_size: f64,
}

/// The `p`-th percentile with linear interpolation — the SAME rule as
/// `compute::percentile` (which is private), so the columns are comparable.
fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let k = (v.len() - 1) as f64 * p;
    let lo = k.floor() as usize;
    let hi = (lo + 1).min(v.len() - 1);
    v[lo] + (v[hi] - v[lo]) * (k - lo as f64)
}

/// Minutes whose spread fractions, converted to pips at `pip_size`.
fn pips(bars: &[&MinuteBar], pip_size: f64) -> Vec<f64> {
    bars.iter()
        .map(|b| b.spread_frac * b.mid_close / pip_size)
        .filter(|p| p.is_finite())
        .collect()
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
    let tz: chrono_tz::Tz = args
        .tz
        .parse()
        .map_err(|e| color_eyre::eyre::eyre!("bad tz {}: {e}", args.tz))?;

    let token = std::env::var("OANDA_TOKEN")
        .or_else(|_| std::env::var("OANDA_API_KEY"))
        .map_err(|_| color_eyre::eyre::eyre!("set OANDA_TOKEN (or OANDA_API_KEY)"))?;
    let client = oanda_client::OandaClient::new(token);

    eprintln!(
        "fetching {} M1 candles, last {} days...",
        args.instrument, args.days
    );
    let minutes =
        spread_baseline_gen::fetch::fetch_oanda_minutes(&client, &args.instrument, args.days, tz)
            .await?;
    eprintln!("got {} minute bars\n", minutes.len());

    // Bucket by schedule-local hour, exactly as `profile_from_minutes` does.
    let mut by_hour: Vec<Vec<&MinuteBar>> = vec![Vec::new(); 24];
    for b in &minutes {
        let h = b.local_hour as usize;
        if h < 24 {
            by_hour[h].push(b);
        }
    }

    // The hour-CLOSING minute only (:59 of the local hour) reconstructs an
    // H1-close sample from the same data — the apples-to-apples comparison.
    let mut close_by_hour: Vec<Vec<&MinuteBar>> = vec![Vec::new(); 24];
    for b in &minutes {
        let h = b.local_hour as usize;
        if h < 24 && b.utc_minute_of_day % 60 == 59 {
            close_by_hour[h].push(b);
        }
    }

    println!(
        "{} — spread in pips, by schedule-local hour",
        args.instrument
    );
    println!("all-minutes = the generator's population; close-only = H1-close sample\n");
    println!(
        "{:>4} {:>7} {:>7} {:>7} {:>7} {:>7} │ {:>6} {:>7} {:>7}",
        "hour", "n", "p50", "p75", "p90", "p99", "n_cl", "cl_p50", "cl_p90"
    );

    for h in 0..24 {
        let all = pips(&by_hour[h], args.pip_size);
        let cl = pips(&close_by_hour[h], args.pip_size);
        if all.is_empty() {
            continue;
        }
        println!(
            "{h:>4} {:>7} {:>7.2} {:>7.2} {:>7.2} {:>7.2} │ {:>6} {:>7.2} {:>7.2}",
            all.len(),
            percentile(&all, 0.50),
            percentile(&all, 0.75),
            percentile(&all, 0.90),
            percentile(&all, 0.99),
            cl.len(),
            percentile(&cl, 0.50),
            percentile(&cl, 0.90),
        );
    }

    Ok(())
}
