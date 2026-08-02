//! **Does the candle-derived spread-hour mask actually differ from the old
//! NY-close rule?** — an agreement census over every baked instrument-hour.
//!
//! # The question
//!
//! Before the candle-derived table, a spread hour was whatever
//! [`ny_clock::is_ny_close_edge`] said: a fixed window around the New York
//! close, identical for every instrument. The table replaced that with a
//! per-instrument mask learned from real spreads. A fair question is whether
//! the learned mask is doing anything the fixed rule wasn't.
//!
//! The corpus cannot answer it. Its fixtures exercise a handful of instruments
//! over a few weeks, so "no fixture changed" conflates *"the rules agree"* with
//! *"these fixtures never hit an hour where they differ"*.
//!
//! # What this measures
//!
//! Every baked instrument × every hour of a full year of UTC hours, comparing
//! [`is_spread_hour`] against [`ny_clock::is_ny_close_edge`]. A full year is
//! deliberate: it spans every DST transition, so an instrument whose schedule
//! is not `ny` disagrees for part of the year purely because the two clocks
//! drift apart — which is itself one of the things the mask buys.
//!
//! Reported per instrument:
//!
//! - `agree`   — both rules say the same thing.
//! - `mask+`   — the mask flags an hour the NY-close rule would NOT.
//! - `mask-`   — the NY-close rule flags an hour the mask does NOT.
//!
//! `mask-` is the interesting column: those are hours the old rule suppressed
//! signals on and the learned mask says are fine to trade.
//!
//! Note the fallback: `is_spread_hour` returns `is_ny_close_edge` verbatim for
//! an instrument with **no row or an empty mask**. Those instruments agree 100%
//! *by construction*, not by measurement, so they are reported separately —
//! folding them into the average would dilute it with rows that cannot disagree.
//!
//! Run: `cargo run -p trade-control-core --example mask_vs_ny_close`

use chrono::{Duration, TimeZone, Utc};
use trade_control_core::spread_blackout::{baked_rows, is_spread_hour};

fn main() {
    // A full year of hourly probes from a fixed start — no `Utc::now()`, so the
    // census is reproducible run to run.
    let start = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("valid start instant");
    let hours: Vec<_> = (0..(365 * 24))
        .map(|h| start + Duration::hours(h))
        .collect();

    let mut rows: Vec<(String, usize, usize, usize, bool)> = Vec::new();
    for (broker, symbol) in baked_rows() {
        // An instrument whose mask is empty falls back to the NY-close rule, so
        // it agrees by construction. Detect that by probing: if the mask never
        // fires across a whole year, there is nothing learned to compare.
        let mut agree = 0usize;
        let mut mask_only = 0usize;
        let mut ny_only = 0usize;
        let mut mask_ever = false;
        for &t in &hours {
            let m = is_spread_hour(symbol, t);
            let n = trade_control_core::ny_clock::is_ny_close_edge(t);
            mask_ever |= m;
            match (m, n) {
                (true, true) | (false, false) => agree += 1,
                (true, false) => mask_only += 1,
                (false, true) => ny_only += 1,
            }
        }
        let learned = mask_only > 0 || ny_only > 0 || mask_ever;
        rows.push((
            format!("{broker} {symbol}"),
            agree,
            mask_only,
            ny_only,
            learned,
        ));
    }

    let total_hours = hours.len();
    let mut disagreeing: Vec<_> = rows
        .iter()
        .filter(|r| r.2 > 0 || r.3 > 0)
        .cloned()
        .collect();
    disagreeing.sort_by_key(|r| std::cmp::Reverse(r.2 + r.3));

    println!(
        "spread-hour mask vs NY-close rule — {} instruments x {total_hours} hours (2026, full year)\n",
        rows.len()
    );
    println!(
        "{:<26} {:>8} {:>8} {:>8} {:>9}",
        "instrument", "agree", "mask+", "mask-", "disagree"
    );
    for (name, agree, mask_only, ny_only, _) in disagreeing.iter().take(25) {
        let pct = 100.0 * (mask_only + ny_only) as f64 / total_hours as f64;
        println!("{name:<26} {agree:>8} {mask_only:>8} {ny_only:>8} {pct:>8.2}%");
    }

    let n_disagree = disagreeing.len();
    let identical = rows.len() - n_disagree;
    let tot_mask_only: usize = rows.iter().map(|r| r.2).sum();
    let tot_ny_only: usize = rows.iter().map(|r| r.3).sum();
    println!(
        "\n{n_disagree} of {} instruments disagree with the NY-close rule at least once.",
        rows.len()
    );
    println!("{identical} agree at every hour of the year.");
    println!(
        "across all instrument-hours: mask+ {tot_mask_only} (mask flags, NY-close would not), \
         mask- {tot_ny_only} (NY-close flags, mask says tradable)"
    );
}
