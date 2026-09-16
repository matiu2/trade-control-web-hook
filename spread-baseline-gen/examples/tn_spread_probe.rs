//! Does the TradeNation adapter hand the baker a real bid/ask spread for this
//! instrument, or a collapsed one?
//!
//! Spain 35 baked as an all-zero row (`vol=0.000000`) while Germany 40 and
//! UK 100 — same code path, same broker — baked normally. The broker itself is
//! not the problem: `tradenation candles --price-type bid|ask --market-id 66670`
//! shows bid 19444.3 / ask 19447.3, a spread of exactly 3.0, at both H1 and M1.
//! So the zero appears somewhere between the HTTP feed and `minutes_from_bidask`.
//!
//! This prints, for one instrument, how many returned candles have
//! `ask_c == bid_c` versus a real spread — the narrowest question that
//! distinguishes "the adapter collapsed the sides" from "the baker mis-computed".
//!
//! Run: cargo run -p spread-baseline-gen --example tn_spread_probe -- --instrument "Spain 35"

use chrono::Duration;
use clap::Parser;
use color_eyre::eyre::{Result, eyre};
use spread_baseline_gen::compute::profile_from_minutes;
use spread_baseline_gen::fetch::minutes_from_bidask;
use trade_control_core::broker::{Broker as _, Granularity};

#[derive(Parser, Debug)]
struct Args {
    /// TradeNation market name, e.g. "Spain 35".
    #[arg(long)]
    instrument: String,
    /// How far back to sample.
    #[arg(long, default_value_t = 2)]
    days: i64,
    /// Schedule timezone to bucket local hours in.
    #[arg(long, default_value = "Europe/Berlin")]
    tz: String,
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
    let session = tradenation_api::login_demo()
        .await
        .map_err(|e| eyre!("TN login_demo: {e}"))?;
    let session_json = serde_json::to_string(&session)?;
    let broker = broker_tradenation::login(&session_json)
        .await
        .ok_or_else(|| eyre!("broker_tradenation::login returned None"))?;
    let adapter = broker_tradenation_adapter::TradeNationAdapter(broker);

    let now = chrono::Utc::now();
    let since = now - Duration::days(args.days);
    let candles = adapter
        .get_bidask_candles(&args.instrument, Granularity::M1, since, now)
        .await
        .map_err(|e| eyre!("get_bidask_candles({}): {e:?}", args.instrument))?;

    let total = candles.len();
    let zero = candles.iter().filter(|c| c.ask_c == c.bid_c).count();
    println!("{} — {total} M1 candles, {zero} with ask_c == bid_c", args.instrument);
    for c in candles.iter().take(3) {
        println!(
            "  {}  mid={:<10} bid_c={:<10} ask_c={:<10} spread={}",
            c.time, c.c, c.bid_c, c.ask_c, c.ask_c - c.bid_c
        );
    }

    // Now run the REAL compute path on those candles and report what the baker
    // would have written. `vol` is the suspect: it is median(|dmid|/mid) over
    // mids resampled one-per-contiguous-LOCAL-HOUR-run, and a part-time market
    // revisits the same local hour on the next day, which does not start a new
    // run.
    let tz: chrono_tz::Tz = args
        .tz
        .parse()
        .map_err(|e| eyre!("bad tz {:?}: {e}", args.tz))?;
    let minutes = minutes_from_bidask(&candles, tz);
    println!("  minute bars: {}", minutes.len());

    let mut runs = 0usize;
    let mut last: Option<u8> = None;
    let mut hourly: Vec<f64> = Vec::new();
    for b in &minutes {
        if last != Some(b.local_hour) {
            runs += 1;
            hourly.push(b.mid_close);
            last = Some(b.local_hour);
        }
    }
    let mut rets: Vec<f64> = hourly
        .windows(2)
        .filter(|w| w[0] > 0.0)
        .map(|w| (w[1] - w[0]).abs() / w[0])
        .collect();
    rets.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let zero_rets = rets.iter().filter(|r| **r == 0.0).count();
    println!(
        "  hour-runs: {runs}, returns: {}, of which EXACTLY ZERO: {zero_rets} ({:.0}%)",
        rets.len(),
        100.0 * zero_rets as f64 / rets.len().max(1) as f64,
    );

    // How many distinct local hours clear MIN_HOUR_MINUTES? `apply_gates` bails
    // to an empty profile (vol: 0.0) when fewer than 12 hours are sampled, which
    // is what a part-time exchange cannot reach.
    let mut per_hour = [0usize; 24];
    for b in &minutes {
        if (b.local_hour as usize) < 24 {
            per_hour[b.local_hour as usize] += 1;
        }
    }
    let sampled: Vec<usize> = (0..24).filter(|h| per_hour[*h] >= 20).collect();
    println!(
        "  local hours with >=20 minutes: {} -> {:?}",
        sampled.len(),
        sampled,
    );

    let profile = profile_from_minutes(&minutes, 0.01);
    println!(
        "  => vol={:.8}  median_ratio={:.3}  n_bars={}",
        profile.vol, profile.median_ratio, profile.n_bars,
    );
    Ok(())
}
