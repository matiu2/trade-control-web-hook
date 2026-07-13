//! Candle-derived spread-baseline generator — Stage 1 (validation-only).
//!
//! Fetches H1 bid/ask candles from OANDA and/or TradeNation for the catalog
//! instruments, computes the per-(broker, instrument) med3 spread-hour mask,
//! writes a committed `spread_baseline_candle.rs` table, and prints a
//! validation report diffing the masks against the known anchors.
//!
//! **Stage 1 does NOT swap the live gate** — it only produces + validates the
//! table. The `core/build.rs` gate-swap is a later stage.
//!
//! Auth: `OANDA_TOKEN` (practice) for OANDA; the default TradeNation demo
//! session for TN (operator-run, not CI).
//!
//! Usage:
//!   generate --brokers oanda,tradenation --out spread_baseline_candle.rs
//!   generate --brokers oanda --only EUR_USD,XAU_USD   # spot-check a few

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::Parser;
use color_eyre::eyre::{Result, WrapErr, eyre};
use tracing::{info, warn};

use spread_baseline_gen::compute::profile_for_instrument;
use spread_baseline_gen::fetch::{bars_from_bidask, fetch_oanda};
use spread_baseline_gen::render::render_table;
use spread_baseline_gen::universe::{WorkItem, work_items};
use spread_baseline_gen::{BaselineRow, Broker};

#[derive(Parser, Debug)]
#[command(about = "Candle-derived per-broker spread-baseline generator (validation-only)")]
struct Args {
    /// Comma-separated brokers to profile: `oanda`, `tradenation`.
    #[arg(long, default_value = "oanda,tradenation")]
    brokers: String,

    /// Output path for the generated table (relative to cwd).
    #[arg(long, default_value = "spread_baseline_candle.rs")]
    out: PathBuf,

    /// Restrict to these broker symbols (comma-separated) — for spot-checks.
    #[arg(long)]
    only: Option<String>,

    /// Include Stock-class instruments (deferred by default in Stage 1).
    #[arg(long)]
    include_stocks: bool,

    /// Print the per-hour ratio table for each instrument (verbose).
    #[arg(long)]
    verbose: bool,

    /// Print the full per-UTC-hour `p90(spread/mid)` + `ratio` table for each
    /// instrument (for threshold calibration). Implies a small `--only` set.
    #[arg(long)]
    dump_hours: bool,
}

/// Known anchor masks (UTC hours) to diff against — proves parity with the
/// sampler + the OANDA scratch validation. `None` list = expected zero.
fn anchors() -> Vec<(Broker, &'static str, Vec<u8>)> {
    vec![
        (Broker::Oanda, "EUR_USD", vec![21]),
        (Broker::Oanda, "XAU_USD", vec![]),
        (Broker::TradeNation, "EUR/USD", vec![21]),
        (Broker::TradeNation, "AUD/CHF", vec![21]),
        (Broker::TradeNation, "Spot Gold", vec![]),
    ]
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let brokers = parse_brokers(&args.brokers)?;
    let only: Option<Vec<String>> = args
        .only
        .as_ref()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect());

    let assets = instrument_lookup::all().map_err(|e| eyre!("instrument-lookup catalog: {e}"))?;
    let mut items = work_items(assets, &brokers, args.include_stocks);
    if let Some(only) = &only {
        items.retain(|i| only.iter().any(|s| s == &i.symbol));
    }
    info!(
        "profiling {} (broker, instrument) pairs across {:?}",
        items.len(),
        brokers
    );

    // Build clients once.
    let oanda = if brokers.contains(&Broker::Oanda) {
        let token = std::env::var("OANDA_TOKEN")
            .wrap_err("OANDA_TOKEN env var required for OANDA fetch")?;
        Some(oanda_client::OandaClient::new(token))
    } else {
        None
    };
    let tn = if brokers.contains(&Broker::TradeNation) {
        Some(acquire_tn().await?)
    } else {
        None
    };

    let mut rows: Vec<BaselineRow> = Vec::new();
    for item in &items {
        match profile_one(item, oanda.as_ref(), tn.as_ref()).await {
            Ok(Some(row)) => {
                if args.verbose {
                    info!(
                        "{} {} => hours {:?} (vol={:.6}, med_ratio={:.2}, n={})",
                        row.broker.as_str(),
                        row.symbol,
                        row.profile.elevated_vec(),
                        row.profile.vol,
                        row.profile.median_ratio,
                        row.profile.n_bars,
                    );
                }
                if args.dump_hours {
                    dump_hours(&row);
                }
                rows.push(row);
            }
            Ok(None) => warn!(
                "{} {}: skipped (no usable bars)",
                item.broker.as_str(),
                item.symbol
            ),
            Err(e) => warn!(
                "{} {}: fetch failed: {e}",
                item.broker.as_str(),
                item.symbol
            ),
        }
    }

    // Write the table.
    let table = render_table(&rows);
    std::fs::write(&args.out, &table).wrap_err_with(|| format!("write {}", args.out.display()))?;
    info!("wrote {} rows to {}", rows.len(), args.out.display());

    // Validation report.
    print_validation(&rows);

    Ok(())
}

/// Profile a single work item against whichever client its broker needs.
async fn profile_one(
    item: &WorkItem,
    oanda: Option<&oanda_client::OandaClient>,
    tn: Option<&broker_tradenation_adapter::TradeNationAdapter>,
) -> Result<Option<BaselineRow>> {
    use trade_control_core::broker::{Broker as _, Granularity};

    let bars = match item.broker {
        Broker::Oanda => {
            let client = oanda.ok_or_else(|| eyre!("OANDA client not built"))?;
            fetch_oanda(client, &item.symbol).await?
        }
        Broker::TradeNation => {
            let broker = tn.ok_or_else(|| eyre!("TN broker not built"))?;
            // ~2000 H1 bars back from now.
            let now = chrono::Utc::now();
            let since =
                now - chrono::Duration::hours(spread_baseline_gen::fetch::CANDLE_COUNT as i64);
            let candles = broker
                .get_bidask_candles(&item.symbol, Granularity::H1, since, now)
                .await
                .map_err(|e| eyre!("tn get_bidask_candles({}): {e:?}", item.symbol))?;
            bars_from_bidask(&candles)
        }
    };

    if bars.is_empty() {
        return Ok(None);
    }
    let profile = profile_for_instrument(&bars);
    Ok(Some(BaselineRow {
        broker: item.broker,
        symbol: item.symbol.clone(),
        display_name: item.display_name.clone(),
        profile,
    }))
}

/// Acquire the default TradeNation demo broker (spread profiles are
/// market-wide, not account-specific, so the default demo session is fine).
async fn acquire_tn() -> Result<broker_tradenation_adapter::TradeNationAdapter> {
    let session = tradenation_api::login_demo()
        .await
        .map_err(|e| eyre!("TN login_demo: {e}"))?;
    let session_json = serde_json::to_string(&session).wrap_err("serialize TN session")?;
    let broker = broker_tradenation::login(&session_json)
        .await
        .ok_or_else(|| eyre!("broker_tradenation::login returned None"))?;
    Ok(broker_tradenation_adapter::TradeNationAdapter(broker))
}

/// Print the full per-UTC-hour `p90(spread/mid)` and `ratio` table for one
/// instrument, marking the elevated hours. For threshold calibration — shows
/// WHY an hour did or didn't clear `3 × median_ratio`.
fn dump_hours(row: &BaselineRow) {
    let p = &row.profile;
    let threshold = spread_baseline_gen::compute::MED_MULT * p.median_ratio;
    println!(
        "\n--- {} {}  vol={:.6}  med_ratio={:.3}  threshold(3x)={:.3}  n={} ---",
        row.broker.as_str(),
        row.symbol,
        p.vol,
        p.median_ratio,
        threshold,
        p.n_bars,
    );
    println!(
        "  {:>3} {:>4} {:>13} {:>7}",
        "UTC", "Bris", "p90_spr/mid", "ratio"
    );
    for h in 0..24usize {
        if p.hour_p90_frac[h] == 0.0 && p.hour_ratio[h] == 0.0 {
            continue; // under-sampled hour
        }
        let flag = if p.elevated_hours & (1 << h) != 0 {
            "  <== SPREAD HOUR"
        } else {
            ""
        };
        println!(
            "  {:>3} {:>4} {:>13.6} {:>7.2}{}",
            h,
            (h + 10) % 24,
            p.hour_p90_frac[h],
            p.hour_ratio[h],
            flag,
        );
    }
}

/// Diff the computed masks against the known anchors and print a pass/fail
/// report. Also print the full per-broker mask list for eyeballing.
fn print_validation(rows: &[BaselineRow]) {
    let by_key: BTreeMap<(&str, &str), &BaselineRow> = rows
        .iter()
        .map(|r| ((r.broker.as_str(), r.symbol.as_str()), r))
        .collect();

    println!("\n===== ANCHOR VALIDATION =====");
    let mut all_ok = true;
    for (broker, symbol, expected) in anchors() {
        match by_key.get(&(broker.as_str(), symbol)) {
            Some(row) => {
                let got = row.profile.elevated_vec();
                let ok = got == expected;
                all_ok &= ok;
                println!(
                    "  [{}] {} {:<10} expected {:?}  got {:?}",
                    if ok { "PASS" } else { "FAIL" },
                    broker.as_str(),
                    symbol,
                    expected,
                    got,
                );
            }
            None => {
                all_ok = false;
                println!(
                    "  [MISS] {} {:<10} not in output (fetch failed?)",
                    broker.as_str(),
                    symbol
                );
            }
        }
    }
    println!(
        "===== {} =====\n",
        if all_ok {
            "ALL ANCHORS PASS"
        } else {
            "ANCHOR MISMATCH — review"
        }
    );

    println!("===== ALL FLAGGED SPREAD HOURS =====");
    let mut flagged = 0usize;
    for r in rows {
        let hours = r.profile.elevated_vec();
        if !hours.is_empty() {
            flagged += 1;
            let bris: Vec<u8> = hours.iter().map(|h| (h + 10) % 24).collect();
            println!(
                "  {:<12} {:<12} UTC {:?}  (Bris {:?})",
                r.broker.as_str(),
                r.symbol,
                hours,
                bris
            );
        }
    }
    println!(
        "  {} of {} instruments have ≥1 spread hour\n",
        flagged,
        rows.len()
    );
}

fn parse_brokers(s: &str) -> Result<Vec<Broker>> {
    let mut out = Vec::new();
    for tok in s.split(',').map(|t| t.trim()).filter(|t| !t.is_empty()) {
        match tok.to_ascii_lowercase().as_str() {
            "oanda" => out.push(Broker::Oanda),
            "tradenation" | "tn" => out.push(Broker::TradeNation),
            other => return Err(eyre!("unknown broker: {other}")),
        }
    }
    if out.is_empty() {
        return Err(eyre!("no brokers specified"));
    }
    Ok(out)
}
