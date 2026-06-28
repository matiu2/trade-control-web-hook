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
/// can't be baked without knowing pip==tick).
#[derive(Deserialize)]
struct Sample {
    instrument: String,
    spread_pips: Option<f64>,
}

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
    let text = std::fs::read_to_string(path).ok()?;
    let samples: Vec<Sample> = serde_yaml::from_str(&text).ok()?;

    let instrument = samples.first()?.instrument.clone();
    let mut pips: Vec<f64> = samples
        .iter()
        .filter_map(|s| s.spread_pips)
        .filter(|p| p.is_finite() && *p > 0.0)
        .collect();
    if pips.is_empty() {
        return None;
    }
    pips.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let low = pips[0];
    let high = pips[pips.len() - 1];
    let median = pips[pips.len() / 2];
    Some((
        instrument,
        Baseline {
            low,
            high,
            median,
            n: pips.len(),
        },
    ))
}

/// Render the generated Rust: a `&[(name, low, high, median, n)]` slice
/// the gate binary-searches at runtime. Sorted by name (BTreeMap iter).
fn render_table(baselines: &BTreeMap<String, Baseline>) -> String {
    let mut s = String::new();
    s.push_str("// @generated by build.rs from spread-sampler-cron samples. Do not edit.\n");
    s.push_str("// (name, low_pips, high_pips, median_pips, n_samples), sorted by name.\n");
    s.push_str("pub static SPREAD_BASELINE: &[(&str, f64, f64, f64, u32)] = &[\n");
    for (name, b) in baselines {
        // Escape the name for a Rust string literal (TN names have spaces,
        // slashes, parens — none of which need escaping except a quote or
        // backslash, which TN names don't contain, but be safe).
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        s.push_str(&format!(
            "    (\"{escaped}\", {:?}, {:?}, {:?}, {}),\n",
            b.low, b.high, b.median, b.n
        ));
    }
    s.push_str("];\n");
    s
}
