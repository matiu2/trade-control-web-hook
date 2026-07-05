//! Compile-time spread-baseline extraction.
//!
//! Reads the per-instrument YAML samples committed by the
//! `spread-sampler-cron` submodule and emits a generated Rust table of
//! per-instrument spread statistics (in **pips**, keyed by TradeNation
//! MarketName — the same `resolved.instrument` the worker's spread-blackout
//! gate compares against). The gate (`spread_blackout.rs`) consults the
//! table to pick a per-instrument threshold instead of the flat 8-pip
//! constant that mis-fired for non-FX instruments (Copper's *normal* spread
//! is ~150 pips, so the flat 8 blocked every legitimate entry).
//!
//! This generation lives in **`core`** (not the worker crate) so the offline
//! replay — which links `core` but not the worker `cdylib` — bakes the same
//! table and applies the same spread-blackout reject as the live worker
//! (`[[strategy_changes_in_both_replayer_and_worker]]`).
//!
//! Runs on the HOST, not wasm — host-native serde_yaml is fine here.
//!
//! Fail-soft by design: if the samples dir is missing (a fresh checkout
//! that hasn't pulled the submodule) or a file is unparseable, we emit an
//! empty table and the gate falls back to the flat constant for every
//! instrument. A missing baseline must never break the build.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Where the committed samples live, relative to this crate root. `core` sits
/// one directory below the worker repo root, and the `spread-sampler-cron`
/// submodule sits beside the repo in the trading-libraries tree — so from
/// `core/` the samples are two levels up (`../../`), one more than the root
/// `build.rs` (`../`).
const SAMPLES_DIR: &str = "../../spread-sampler-cron/samples";

/// One sampled quote — mirrors `spread_sampler_cron::sample::Sample`, but
/// we only deserialise the fields the baseline needs. `spread_pips` is
/// present only for instruments `instrument-lookup` catalogues; we skip
/// any sample without it (the gate works in pips, so a tick-only sample
/// can't be baked without knowing pip==tick). `at` is the sample's UTC
/// instant (serde reads the RFC3339 `...Z` string); we bucket by its hour
/// to learn each instrument's per-hour spread profile (the spread-hour
/// timing layer — see `render_table` / `spread_hours`).
#[derive(Deserialize)]
struct Sample {
    instrument: String,
    at: chrono::DateTime<chrono::Utc>,
    spread_pips: Option<f64>,
}

/// An hour `h` counts as "elevated" (a spread hour) when EITHER its median
/// OR its p90 runs above the instrument's OWN quietest-hour median by the
/// respective multiple below.
///
/// Comparing to the *quietest hour*, not the all-day median, is what
/// separates a real localized spread hour from a flat-but-fat-tailed
/// instrument whose spread is noisy at *every* hour (many equities). A ratio
/// vs the all-day median let those through — some hour always ranks "top" on
/// tail noise. Vs the quiet baseline, a genuinely flat instrument flags
/// nothing.
///
/// Two arms because the 2026-07-05 sampler analysis (1183 instruments)
/// showed the two real spread-hour shapes live in different statistics:
///
/// - **Structural shift (median arm, [`QUIET_MULT`]).** Spot Gold's overnight
///   block: median 40p daytime → 60p overnight. The whole block's *median*
///   lifts, so `median(h) ≥ 1.5 × quiet` catches it.
/// - **Tail spike (p90 arm, [`TAIL_MULT`]).** EUR/USD 21:00 UTC: *median*
///   stays 0.5 (identical to every hour) but p90 jumps to 5.0 (10×). A
///   median test is blind to it; only `p90(h) ≥ 3 × quiet` fires. Other
///   hours' p90 sits ≤1.4× quiet, so 3× isolates the spike with margin.
///
/// An hour is elevated iff EITHER arm trips.
const QUIET_MULT: f64 = 1.5;

/// Tail arm of the elevated-hour test — see [`QUIET_MULT`]. A p90 at least
/// this multiple of the quiet-hour median marks a tail spike (EUR/USD's
/// 21:00 NY-close blowout, invisible to the median arm). 3× clears the
/// ≤1.4× resting-tail noise floor by a wide margin.
const TAIL_MULT: f64 = 3.0;

/// The 90th-percentile spread of an hour is what we widen an open stop by
/// when that hour is elevated — the "typical high" of the spike, robust to
/// a lone freak print (which `max` would chase). Baked per elevated hour.
const WIDEN_PERCENTILE: f64 = 0.90;

/// Minimum samples in an hour bucket before we trust its median/p90. Fewer
/// than this ⇒ treat the hour as unknown (not elevated) rather than let a
/// 1–2-sample bucket declare a spread hour on noise.
const MIN_HOUR_SAMPLES: usize = 3;

/// Per-instrument baseline figures, in pips.
struct Baseline {
    /// Smallest observed spread (the liquid-hours "normal" floor).
    low: f64,
    /// Largest observed spread (the spike ceiling).
    high: f64,
    /// Median observed spread (the resting "normal").
    median: f64,
    /// How many samples contributed (for visibility / confidence).
    n: usize,
    /// Bit `h` set ⇒ UTC hour `h` is a spread hour for this instrument.
    elevated_hours: u32,
    /// Per-UTC-hour widen size in pips (that hour's p90 when elevated, else
    /// 0.0). Indexed by hour 0..23. The System-2 widen reads `now.hour()`.
    hour_widen_pips: [f64; 24],
}

fn main() {
    let dir = PathBuf::from(SAMPLES_DIR);
    // Re-run if any sample file changes (cron commits append to these).
    println!("cargo:rerun-if-changed={SAMPLES_DIR}");
    println!("cargo:rerun-if-changed=build.rs");

    let baselines = read_baselines(&dir);
    let generated = render_table(&baselines);

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    std::fs::write(out.join("spread_baseline.rs"), generated)
        .expect("write generated spread_baseline.rs");
}

/// Walk the samples dir and fold each instrument's samples into a
/// [`Baseline`]. Returns an empty map (→ flat-constant fallback for all)
/// if the dir is absent or empty.
fn read_baselines(dir: &Path) -> BTreeMap<String, Baseline> {
    let mut out = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            // No samples checked out — emit an empty table, gate falls
            // back to the flat constant. Not a build error.
            println!(
                "cargo:warning=spread-baseline: no samples dir at {SAMPLES_DIR}; baking empty table (flat-8 fallback)"
            );
            return out;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        if let Some(b) = baseline_for_file(&path) {
            out.insert(b.0, b.1);
        }
    }
    out
}

/// Parse one YAML file into a `(instrument, Baseline)`. `None` when the
/// file is unparseable or has no pip-bearing samples.
fn baseline_for_file(path: &Path) -> Option<(String, Baseline)> {
    use chrono::Timelike;

    let text = std::fs::read_to_string(path).ok()?;
    let samples: Vec<Sample> = serde_yaml::from_str(&text).ok()?;

    let instrument = samples.first()?.instrument.clone();

    // All finite positive spreads (whole-day baseline) AND the per-hour
    // buckets, in one pass.
    let mut pips: Vec<f64> = Vec::with_capacity(samples.len());
    let mut by_hour: [Vec<f64>; 24] = Default::default();
    for s in &samples {
        let Some(p) = s.spread_pips.filter(|p| p.is_finite() && *p > 0.0) else {
            continue;
        };
        pips.push(p);
        by_hour[s.at.hour() as usize].push(p);
    }
    if pips.is_empty() {
        return None;
    }
    pips.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let low = pips[0];
    let high = pips[pips.len() - 1];
    let median = pips[pips.len() / 2];

    let (elevated_hours, hour_widen_pips) = spread_hours(&mut by_hour);

    Some((
        instrument,
        Baseline {
            low,
            high,
            median,
            n: pips.len(),
            elevated_hours,
            hour_widen_pips,
        },
    ))
}

/// The `p`-th percentile of a **sorted** slice (linear interpolation).
/// `p` in `[0.0, 1.0]`. Empty ⇒ 0.0 (caller never calls on empty via the
/// `MIN_HOUR_SAMPLES` gate, but keep it total).
fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let k = (sorted.len() - 1) as f64 * p;
    let lo = k.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    let frac = k - lo as f64;
    sorted[lo] + (sorted[hi] - sorted[lo]) * frac
}

/// Compute the per-instrument spread-hour profile from its per-UTC-hour
/// spread buckets. Sorts each bucket in place (hence `&mut`).
///
/// Returns `(elevated_hours_bitmask, hour_widen_pips[24])`:
/// - an hour is elevated iff it has ≥[`MIN_HOUR_SAMPLES`] samples and its
///   median ≥ [`QUIET_MULT`] × the quietest qualifying hour's median;
/// - the widen size for an elevated hour is its [`WIDEN_PERCENTILE`] (p90);
///   non-elevated hours bake 0.0.
///
/// Fail-open: if fewer than a handful of hours have data (a barely-sampled
/// instrument) the quietest-baseline comparison is meaningless, so we bake
/// mask 0 → the gate falls back to `is_ny_close_edge` for that instrument.
fn spread_hours(by_hour: &mut [Vec<f64>; 24]) -> (u32, [f64; 24]) {
    // Per-hour median AND p90 for the hours that clear the sample-count gate.
    // (Sort once; both stats read the sorted bucket.)
    let mut hour_median: [Option<f64>; 24] = [None; 24];
    let mut hour_p90: [f64; 24] = [0.0; 24];
    for (h, bucket) in by_hour.iter_mut().enumerate() {
        if bucket.len() < MIN_HOUR_SAMPLES {
            continue;
        }
        bucket.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        hour_median[h] = Some(bucket[bucket.len() / 2]);
        hour_p90[h] = percentile_sorted(bucket, WIDEN_PERCENTILE);
    }

    // Need a spread of hours to establish a "quiet baseline" at all. With
    // too few populated hours the min is not a meaningful quiet floor.
    let populated = hour_median.iter().filter(|m| m.is_some()).count();
    if populated < 12 {
        return (0, [0.0; 24]);
    }
    let quiet = hour_median
        .iter()
        .filter_map(|m| *m)
        .fold(f64::INFINITY, f64::min);
    if !(quiet > 0.0 && quiet.is_finite()) {
        return (0, [0.0; 24]);
    }

    let mut mask: u32 = 0;
    let mut widen = [0.0_f64; 24];
    for h in 0..24 {
        let Some(m) = hour_median[h] else { continue };
        // Elevated iff EITHER the median lifts (structural, Gold) OR the p90
        // spikes (tail, EUR/USD 21:00) — see QUIET_MULT / TAIL_MULT.
        let structural = m >= quiet * QUIET_MULT;
        let tail = hour_p90[h] >= quiet * TAIL_MULT;
        if structural || tail {
            mask |= 1 << h;
            // Widen by the p90 either way — for a tail-spike hour that IS the
            // spike size; for a structural hour it's the typical high.
            widen[h] = hour_p90[h];
        }
    }
    (mask, widen)
}

/// Render the generated Rust: a slice of
/// `(name, low, high, median, n, elevated_hours, hour_widen_pips[24])` the
/// gate binary-searches at runtime. Sorted by name (BTreeMap iter).
///
/// `elevated_hours` is a u32 bitmask (bit `h` = UTC hour `h` is a spread
/// hour); `hour_widen_pips` is the per-hour widen size (that hour's p90 when
/// elevated, 0.0 otherwise). Both are 0 for an instrument with no learnable
/// spread hours (→ `is_ny_close_edge` fallback at the consumer).
fn render_table(baselines: &BTreeMap<String, Baseline>) -> String {
    let mut s = String::new();
    s.push_str("// @generated by build.rs from spread-sampler-cron samples. Do not edit.\n");
    s.push_str(
        "// (name, low_pips, high_pips, median_pips, n_samples, elevated_hours_mask, \
         hour_widen_pips[24]), sorted by name.\n",
    );
    // The row tuple is wide by design (a fixed generated table, not an API
    // surface); silence the type-complexity lint on the generated static.
    s.push_str("#[allow(clippy::type_complexity)]\n");
    s.push_str("pub static SPREAD_BASELINE: &[(&str, f64, f64, f64, u32, u32, [f64; 24])] = &[\n");
    for (name, b) in baselines {
        // Escape the name for a Rust string literal (TN names have spaces,
        // slashes, parens — none of which need escaping except a quote or
        // backslash, which TN names don't contain, but be safe).
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        let widen = b
            .hour_widen_pips
            .iter()
            .map(|p| format!("{p:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        s.push_str(&format!(
            "    (\"{escaped}\", {:?}, {:?}, {:?}, {}, {}, [{widen}]),\n",
            b.low, b.high, b.median, b.n, b.elevated_hours
        ));
    }
    s.push_str("];\n");
    s
}
