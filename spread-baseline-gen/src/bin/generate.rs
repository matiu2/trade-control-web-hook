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

use spread_baseline_gen::compute::profile_from_minutes;
use spread_baseline_gen::fetch::{fetch_oanda_minutes, minutes_from_bidask};
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

    /// Lookback window in days for the minute-level fetch. As of Stage 2 the
    /// mask buckets by the asset's schedule-LOCAL hour, so a window spanning a
    /// DST transition NO LONGER smears the spike across two UTC hours — a full
    /// year lands in the same local hour. Longer windows are now fine (and
    /// preferable for stable buckets). Default 90d.
    #[arg(long, default_value_t = 90)]
    days: i64,

    /// Print the per-hour ratio table for each instrument (verbose).
    #[arg(long)]
    verbose: bool,

    /// Print the full per-UTC-hour `p90(spread/mid)` + `ratio` table for each
    /// instrument (for threshold calibration). Implies a small `--only` set.
    #[arg(long)]
    dump_hours: bool,
}

/// Known anchor masks in **schedule-LOCAL hours** to diff against. Stage 2
/// buckets by the asset's schedule-local hour, so the NY-close spike that used
/// to sit at UTC 21 (summer) / 22 (winter) now lands at local hour **17**
/// (5pm New York) year-round — that DST-invariance is the whole point. `None`
/// list = expected zero.
fn anchors() -> Vec<(Broker, &'static str, Vec<u8>)> {
    vec![
        (Broker::Oanda, "EUR_USD", vec![17]),
        (Broker::Oanda, "XAU_USD", vec![]),
        (Broker::TradeNation, "EUR/USD", vec![17]),
        (Broker::TradeNation, "AUD/CHF", vec![17]),
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
        match profile_one(item, args.days, oanda.as_ref(), tn.as_ref()).await {
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
    // Schedule cross-check: does each flagged LOCAL hour match its schedule?
    print_cross_check(&rows);

    Ok(())
}

/// Resolve a work item's spread-schedule tz for local-hour bucketing.
///
/// - `none`/unknown FK (`spread_schedule_tz == None`) ⇒ `Ok(None)`: the asset
///   has no spread hour, so it's SKIPPED for profiling (caller returns
///   `Ok(None)`).
/// - a tz string that fails to parse ⇒ WARN and default to `America/New_York`,
///   so an FX asset never silently drops from the table on a bad FK.
fn resolve_tz(item: &WorkItem) -> Option<chrono_tz::Tz> {
    let tz_str = item.spread_schedule_tz.as_deref()?;
    match tz_str.parse::<chrono_tz::Tz>() {
        Ok(tz) => Some(tz),
        Err(e) => {
            warn!(
                "{} {}: schedule '{}' tz '{tz_str}' failed to parse ({e}); \
                 defaulting to America/New_York",
                item.broker.as_str(),
                item.symbol,
                item.spread_schedule,
            );
            Some(chrono_tz::America::New_York)
        }
    }
}

/// Profile a single work item against whichever client its broker needs, using
/// the **minute-level** (bleed-resistant) path over the last `days`. Buckets by
/// the asset's schedule-LOCAL hour (DST-invariant); an asset whose schedule is
/// `none` is skipped (`Ok(None)`).
async fn profile_one(
    item: &WorkItem,
    days: i64,
    oanda: Option<&oanda_client::OandaClient>,
    tn: Option<&broker_tradenation_adapter::TradeNationAdapter>,
) -> Result<Option<BaselineRow>> {
    use trade_control_core::broker::{Broker as _, Granularity};

    let Some(tz) = resolve_tz(item) else {
        info!(
            "{} {}: schedule '{}' has no spread hour — skipped",
            item.broker.as_str(),
            item.symbol,
            item.spread_schedule,
        );
        return Ok(None);
    };

    let bars = match item.broker {
        Broker::Oanda => {
            let client = oanda.ok_or_else(|| eyre!("OANDA client not built"))?;
            fetch_oanda_minutes(client, &item.symbol, days, tz).await?
        }
        Broker::TradeNation => {
            let broker = tn.ok_or_else(|| eyre!("TN broker not built"))?;
            let now = chrono::Utc::now();
            let since = now - chrono::Duration::days(days);
            // The adapter now pages M1 in ≤1000-bar chunks, so a multi-day
            // window returns full history (was capped at ~1 day).
            let candles = broker
                .get_bidask_candles(&item.symbol, Granularity::M1, since, now)
                .await
                .map_err(|e| eyre!("tn get_bidask_candles({}): {e:?}", item.symbol))?;
            minutes_from_bidask(&candles, tz)
        }
    };

    if bars.is_empty() {
        return Ok(None);
    }
    let profile = profile_from_minutes(&bars);
    Ok(Some(BaselineRow {
        broker: item.broker,
        symbol: item.symbol.clone(),
        display_name: item.display_name.clone(),
        spread_schedule: item.spread_schedule.clone(),
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
    println!("  {:>5} {:>13} {:>7}", "LOCAL", "p90_spr/mid", "ratio");
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
            "  {:>5} {:>13.6} {:>7.2}{}",
            h, p.hour_p90_frac[h], p.hour_ratio[h], flag,
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
            println!(
                "  {:<12} {:<12} LOCAL {:?}  (schedule '{}')",
                r.broker.as_str(),
                r.symbol,
                hours,
                r.spread_schedule,
            );
        }
    }
    let insufficient = rows
        .iter()
        .filter(|r| {
            matches!(
                r.profile.review,
                spread_baseline_gen::ReviewStatus::InsufficientData
            )
        })
        .count();
    println!(
        "  {} of {} instruments have ≥1 spread hour ({} reviewed-flat, \
         {} insufficient-data)\n",
        flagged,
        rows.len(),
        rows.len() - flagged - insufficient,
        insufficient,
    );
    if insufficient > 0 {
        println!("  INSUFFICIENT DATA (mask=0, gate falls back to NY-close-edge):");
        for r in rows.iter().filter(|r| {
            matches!(
                r.profile.review,
                spread_baseline_gen::ReviewStatus::InsufficientData
            )
        }) {
            println!(
                "    {:<12} {:<12} n={}",
                r.broker.as_str(),
                r.symbol,
                r.profile.n_bars
            );
        }
        println!();
    }
}

/// The expected LOCAL spread hour(s) for a schedule, used by the cross-check.
/// A misassigned schedule shows up as a flagged hour far from these.
///
/// - `ny` → 17 (5pm New-York rollover, the FX/gold/US-index liquidity vacuum).
/// - index/exchange schedules → their session edges (open/close, local). These
///   are soft: a flag a few hours off is WARNed, not failed, because index
///   spread hours are less crisply the "one" hour an FX rollover is.
fn expected_local_hours(schedule: &str) -> Vec<u8> {
    match schedule {
        "ny" => vec![17],
        // Exchange schedules: plausible edges are the local open and close.
        // These are hand-set from the cash-session boundaries; the cross-check
        // only WARNs when a flag is far from ALL of them.
        "london" => vec![8, 16],
        "frankfurt" => vec![9, 17],
        "zurich" => vec![9, 17],
        "sydney" => vec![10, 16],
        "tokyo" => vec![9, 15],
        "hongkong" => vec![9, 16],
        "singapore" => vec![9, 17],
        "johannesburg" => vec![9, 17],
        _ => Vec::new(), // unknown / none — no expectation
    }
}

/// The verdict for one instrument's flagged local hour against its schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrossCheck {
    /// No flagged hour (nothing to check) or no expectation for the schedule.
    NotApplicable,
    /// The flagged hour sits at (or within tolerance of) an expected hour.
    AtExpected,
    /// The flagged hour is off — a possible schedule misassignment.
    Mismatch,
}

/// Cross-check a single instrument's flagged LOCAL hours against its schedule's
/// expected hours. `ny` uses a tight ±1 tolerance (the rollover is a crisp
/// hour); index schedules use ±2 around any plausible session edge. An empty
/// mask or an expectation-less schedule ⇒ `NotApplicable`.
fn cross_check(schedule: &str, flagged: &[u8]) -> CrossCheck {
    if flagged.is_empty() {
        return CrossCheck::NotApplicable;
    }
    let expected = expected_local_hours(schedule);
    if expected.is_empty() {
        return CrossCheck::NotApplicable;
    }
    let tol: i16 = if schedule == "ny" { 1 } else { 2 };
    // Circular distance on a 24-hour clock so 23↔0 is 1 hour, not 23.
    let near = |h: u8| {
        expected.iter().any(|&e| {
            let d = (h as i16 - e as i16).rem_euclid(24);
            d.min(24 - d) <= tol
        })
    };
    if flagged.iter().all(|&h| near(h)) {
        CrossCheck::AtExpected
    } else {
        CrossCheck::Mismatch
    }
}

/// Run the schedule cross-check over all rows and print a summary. Soft: it
/// REPORTS mismatches (a misassigned schedule surfaces as a spike in the wrong
/// local hour) rather than aborting the run.
fn print_cross_check(rows: &[BaselineRow]) {
    println!("===== SCHEDULE CROSS-CHECK (flagged LOCAL hour vs schedule) =====");
    let mut flagged = 0usize;
    let mut at_expected = 0usize;
    let mut mismatches: Vec<String> = Vec::new();
    for r in rows {
        let hours = r.profile.elevated_vec();
        if hours.is_empty() {
            continue;
        }
        flagged += 1;
        match cross_check(&r.spread_schedule, &hours) {
            CrossCheck::AtExpected => at_expected += 1,
            CrossCheck::NotApplicable => {} // no expectation to judge against
            CrossCheck::Mismatch => {
                let expected = expected_local_hours(&r.spread_schedule);
                let entry = format!(
                    "{} {} sched='{}' flagged local {:?} expected ~{:?}",
                    r.broker.as_str(),
                    r.symbol,
                    r.spread_schedule,
                    hours,
                    expected,
                );
                warn!("cross-check MISMATCH: {entry}");
                mismatches.push(entry);
            }
        }
    }
    println!(
        "cross-check: {} instruments, {} flagged, {} at expected local hour, \
         {} mismatches: {:?}",
        rows.len(),
        flagged,
        at_expected,
        mismatches.len(),
        mismatches,
    );
    println!();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_check_ny_spike_at_17_is_expected() {
        // The DST-invariant NY-close spike must land at local hour 17.
        assert_eq!(cross_check("ny", &[17]), CrossCheck::AtExpected);
        // ±1 tolerance: 16 and 18 also pass.
        assert_eq!(cross_check("ny", &[16]), CrossCheck::AtExpected);
        assert_eq!(cross_check("ny", &[18]), CrossCheck::AtExpected);
    }

    #[test]
    fn cross_check_ny_spike_off_hour_is_mismatch() {
        // A UTC-bucketed (un-shifted) NY spike would land at 21/22 — the exact
        // bug Stage 2 fixes. If it shows there, the schedule/tz is misapplied.
        assert_eq!(cross_check("ny", &[21]), CrossCheck::Mismatch);
        assert_eq!(cross_check("ny", &[22]), CrossCheck::Mismatch);
    }

    #[test]
    fn cross_check_empty_mask_is_not_applicable() {
        assert_eq!(cross_check("ny", &[]), CrossCheck::NotApplicable);
    }

    #[test]
    fn cross_check_unknown_schedule_is_not_applicable() {
        // `none` / unrecognised schedules have no expectation ⇒ never a mismatch.
        assert_eq!(cross_check("none", &[3]), CrossCheck::NotApplicable);
        assert_eq!(cross_check("mystery", &[3]), CrossCheck::NotApplicable);
    }

    #[test]
    fn cross_check_index_edge_within_tolerance() {
        // Frankfurt cash open ~09:00 local; a flag at 08 is within ±2.
        assert_eq!(cross_check("frankfurt", &[8]), CrossCheck::AtExpected);
        // A flag nowhere near an edge is a mismatch (WARN, not fail).
        assert_eq!(cross_check("frankfurt", &[2]), CrossCheck::Mismatch);
    }

    #[test]
    fn cross_check_wraps_around_midnight() {
        // Circular distance: an expected 23 and a flag at 0 are 1 hour apart.
        // sydney expects [10,16]; a spike at 23 is >2 from both ⇒ mismatch,
        // but this asserts the wrap math via ny (17) vs a 0-hour flag being far.
        assert_eq!(cross_check("ny", &[0]), CrossCheck::Mismatch);
    }
}
