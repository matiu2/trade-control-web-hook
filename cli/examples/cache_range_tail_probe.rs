//! Probe: does a WIDER `from` (same `to`) change the TAIL of a candle-cache
//! range fetch?
//!
//! Motivation (BUG-warmup-backoff-shrinks-live-window.md): the replay's
//! `pull_with_warmup` widens `pull_from` and retries until the warm-up prefix is
//! deep enough. `pull_end` never moves, so the number of candles at/after the
//! live `start` must be invariant across attempts. Observed live counts were
//! 153, 153, **135**, 135 — one widening silently dropped 18 tail bars, and the
//! replay then scored a different live window (Net R −0.40 vs −3.00 on the same
//! plan and candles).
//!
//! This isolates the question from all replay logic: call
//! `get_candles_range_bid_ask` several times with the SAME `to` and
//! progressively earlier `from`, then compare the tails.
//!
//! An invariant range API must satisfy: for any `from1 < from2 <= to`, the
//! candles in `[from2, to]` returned by the `from1` call are IDENTICAL to those
//! returned by the `from2` call. Widening the look-back may only ADD older bars.
//!
//! Run:
//!   cargo run -p trade-control-cli --example cache_range_tail_probe --release

use candle_cache::{CacheClient, CacheConfig};
use candle_model::Granularity;
use chrono::{DateTime, Duration, FixedOffset, Utc};
use tradenation_api::TradeNationClient;

/// The reproduction case: Coffee 15m, the window the Coffee M15 replays used.
const SYMBOL: &str = "Coffee";
const GRAN: Granularity = Granularity::FifteenMinutes;
/// Fixed right edge — every call shares this, so the tail must not move.
const TO: &str = "2026-07-23T13:59:00Z";
/// The live-window left edge the replay measured `live=` against.
const LIVE_START: &str = "2026-07-19T20:00:00Z";
/// The EXACT `warmup_from` values the failing back-off loop used (Brisbane in
/// the log, UTC here). Note attempts 2/3 are RAGGED (…:54:38) — not bar-aligned.
const EXACT_FROMS: [&str; 4] = [
    "2026-07-17T18:00:00Z", // attempt 0  (2026-07-18 04:00 +10)
    "2026-07-15T03:30:00Z", // attempt 1  (2026-07-15 13:30 +10)
    "2026-07-11T16:54:38Z", // attempt 2  (2026-07-12 02:54:38 +10)  <- ragged
    "2026-07-09T14:54:38Z", // attempt 3  (2026-07-10 00:54:38 +10)  <- ragged
];

fn ts(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().expect("valid RFC3339")
}

fn fx(t: DateTime<Utc>) -> DateTime<FixedOffset> {
    t.fixed_offset()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let to = ts(TO);
    let live_start = ts(LIVE_START);

    let cache_dir = dirs_next_cache().join("candle_cache_tradenation");
    let config = CacheConfig::default().with_cache_dir(cache_dir);
    let client = CacheClient::new(config, tradenation_source()?).await?;

    println!("probe: symbol={SYMBOL} gran={GRAN:?}");
    println!("       to        = {to}  (FIXED for every call)");
    println!("       live_start= {live_start}\n");

    // Keep each call's tail (bars at/after `live_start`) so we can diff them.
    let mut baseline: Option<Vec<(DateTime<Utc>, f64, f64)>> = None;

    for spec in EXACT_FROMS {
        let from = ts(spec);
        let series = client
            .get_candles_range_bid_ask(SYMBOL, fx(from), fx(to), GRAN)
            .await?;

        let all: Vec<_> = series.candles.iter().collect();
        let tail: Vec<(DateTime<Utc>, f64, f64)> = all
            .iter()
            .filter(|c| c.timestamp.with_timezone(&Utc) >= live_start)
            .map(|c| (c.timestamp.with_timezone(&Utc), c.high, c.low))
            .collect();
        let warm = all.len() - tail.len();

        print!(
            "from={from}  total={:>5}  warmup={:>5}  live={:>5}",
            all.len(),
            warm,
            tail.len()
        );

        match &baseline {
            None => {
                println!("   <- baseline");
                baseline = Some(tail);
            }
            Some(base) => {
                if tail.len() != base.len() {
                    println!(
                        "   *** LIVE COUNT CHANGED: {} -> {} (widening `from` must never \
                         alter the tail) ***",
                        base.len(),
                        tail.len()
                    );
                    report_tail_diff(base, &tail);
                } else if tail != *base {
                    println!("   *** SAME COUNT but DIFFERENT VALUES ***");
                    report_tail_diff(base, &tail);
                } else {
                    println!("   tail identical ✓");
                }
            }
        }
    }

    println!(
        "\nInvariant under test: for from1 < from2 <= to, the bars in [live_start, to] must be\n\
         identical across calls. Any '***' line above is a candle-cache range defect."
    );
    Ok(())
}

/// Print the first few timestamps present in one tail but not the other, so a
/// drop is attributable to specific bars rather than just a count.
fn report_tail_diff(base: &[(DateTime<Utc>, f64, f64)], other: &[(DateTime<Utc>, f64, f64)]) {
    let base_times: std::collections::BTreeSet<_> = base.iter().map(|c| c.0).collect();
    let other_times: std::collections::BTreeSet<_> = other.iter().map(|c| c.0).collect();

    println!(
        "      base tail spans {:?} .. {:?}",
        base.first().map(|c| c.0),
        base.last().map(|c| c.0)
    );
    println!(
        "      this tail spans {:?} .. {:?}",
        other.first().map(|c| c.0),
        other.last().map(|c| c.0)
    );
    println!(
        "      base: {} rows, {} distinct ts   |   this: {} rows, {} distinct ts",
        base.len(), base_times.len(), other.len(), other_times.len()
    );
    // Largest inter-bar gaps in each, to spot a hole in the middle.
    for (label, v) in [("base", base), ("this", other)] {
        let mut gaps: Vec<_> = v.windows(2).map(|w| (w[1].0 - w[0].0, w[0].0)).collect();
        gaps.sort_by_key(|g| std::cmp::Reverse(g.0));
        let top: Vec<String> = gaps.iter().take(3)
            .map(|(d, at)| format!("{}min after {at}", d.num_minutes())).collect();
        println!("      {label} largest gaps: {}", top.join(", "));
    }
    let missing: Vec<_> = base_times.difference(&other_times).take(8).collect();
    let added: Vec<_> = other_times.difference(&base_times).take(8).collect();
    if !missing.is_empty() {
        println!("      dropped vs baseline (first {}): {missing:?}", missing.len());
    }
    if !added.is_empty() {
        println!("      added   vs baseline (first {}): {added:?}", added.len());
    }
    // Same-timestamp value drift is a different (worse) failure than a drop.
    let drifted: Vec<_> = base
        .iter()
        .filter_map(|(t, h, l)| {
            other
                .iter()
                .find(|(t2, _, _)| t2 == t)
                .filter(|(_, h2, l2)| h2 != h || l2 != l)
                .map(|(_, h2, l2)| (*t, (*h, *l), (*h2, *l2)))
        })
        .take(5)
        .collect();
    if !drifted.is_empty() {
        println!("      VALUE DRIFT at same timestamps (first {}): {drifted:?}", drifted.len());
    }
}

/// Mirror the replay's cache dir resolution (`~/.cache`).
fn dirs_next_cache() -> std::path::PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var("HOME").expect("HOME set")).join(".cache")
        })
}

/// The TradeNation cache source, same construction the replay uses
/// (`replay_candles/candles.rs::tradenation_source`).
fn tradenation_source() -> Result<TradeNationClient, Box<dyn std::error::Error>> {
    let kind = std::env::var("TN_ACCOUNT_TYPE").unwrap_or_else(|_| "demo".to_string());
    match kind.to_ascii_lowercase().as_str() {
        "live" => Ok(TradeNationClient::new(
            std::env::var("TN_USERNAME")?,
            std::env::var("TN_PASSWORD")?,
        )),
        _ => Ok(TradeNationClient::new_demo()),
    }
}
