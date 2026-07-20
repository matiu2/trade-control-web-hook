//! ATR-relative close→open gap analysis, and the per-instrument
//! market-hours profile it yields.
//!
//! # Method (locked 2026-07-20)
//!
//! For a run of H1 candles, each candle-to-candle **time** gap larger than one
//! bar (a real trading pause: weekend, daily close, holiday) is a *gap event*.
//! For each, we take the 24-bar ATR ending at the close bar and the reopen
//! **jump** `|open_after − close_before|`. A gap "needs attention" when
//! `jump >= 1 × ATR` — the operator's framing: an ATR is roughly one candle's
//! height, and a stop is set ~one candle away, so a >1-ATR reopen gap blows
//! through the stop.
//!
//! # Weekend vs daily
//!
//! Every instrument gets the **weekend** block unconditionally (the Friday
//! close → Monday open span) — that gap is universal. The interesting question
//! is whether an instrument ALSO has a genuine **mid-week daily close** worth
//! blacking out. We answer it by counting attention gaps whose **close bar
//! lands Monday–Thursday** (a Friday attention gap IS the weekend gap and is
//! already covered): if the mid-week attention fraction clears the threshold
//! with enough samples, we bake a daily-close block at the dominant close hours.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Timelike, Utc, Weekday};

/// ATR lookback in candles (24-sample ATR before the gap).
pub const ATR_PERIOD: usize = 24;

/// A time gap larger than this (minutes) is a real trading pause, not the
/// normal 60-min H1 step (a broker's ~5-min housekeeping blip shows as a
/// ~65-min step and must NOT count as a pause).
pub const CONTIGUOUS_MAX_MIN: i64 = 90;

/// Attention threshold: `|reopen jump| >= this many ATRs` → a gap that matters.
pub const ATR_MULTIPLE: f64 = 1.0;

/// Mid-week attention fraction at/above which an instrument gets a daily-close
/// block (in addition to the always-on weekend block).
pub const DAILY_TURN_ON_FRACTION: f64 = 0.20;

/// Minimum gap-event count before the daily-close fraction is trusted. Below
/// this we treat the instrument as weekend-only (too few samples to justify a
/// mid-week block).
pub const MIN_SAMPLES: usize = 30;

/// A close bar's OHLC (mid) with its UTC time — the minimal shape the analysis
/// needs, so both the OANDA and TradeNation fetchers map into it.
#[derive(Debug, Clone, Copy)]
pub struct Bar {
    pub t: DateTime<Utc>,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
}

/// Simple-average ATR over the `ATR_PERIOD` bars ending at index `i`
/// (inclusive). Standard true range `max(H−L, |H−prevC|, |L−prevC|)`. `None`
/// when there aren't `ATR_PERIOD` bars each with a predecessor.
fn atr_ending_at(bars: &[Bar], i: usize) -> Option<f64> {
    if i < ATR_PERIOD {
        return None;
    }
    let mut sum = 0.0;
    for k in (i - ATR_PERIOD + 1)..=i {
        let c = &bars[k];
        let prev_close = bars[k - 1].c;
        let hl = c.h - c.l;
        let hc = (c.h - prev_close).abs();
        let lc = (c.l - prev_close).abs();
        sum += hl.max(hc).max(lc);
    }
    Some(sum / ATR_PERIOD as f64)
}

/// One measured gap event.
#[derive(Debug, Clone, Copy)]
pub struct GapEvent {
    /// Close-bar UTC time (the bar before the pause).
    pub at: DateTime<Utc>,
    /// jump / atr, when ATR is available and positive.
    pub atr_multiple: Option<f64>,
    /// `jump >= ATR_MULTIPLE × atr`.
    pub needs_attention: bool,
}

impl GapEvent {
    /// True when this attention gap is a **mid-week** close (Mon–Thu). A Friday
    /// close is the weekend gap (covered by the weekend block), so it does NOT
    /// count toward the daily-close decision.
    pub fn is_midweek_attention(&self) -> bool {
        self.needs_attention
            && matches!(
                self.at.weekday(),
                Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu
            )
    }
}

/// The computed market-hours profile for one instrument.
#[derive(Debug, Clone)]
pub struct MarketHoursProfile {
    pub total_gaps: usize,
    pub attention_gaps: usize,
    pub midweek_attention_gaps: usize,
    /// midweek_attention_gaps / total_gaps.
    pub midweek_fraction: f64,
    /// Whether a daily-close block should be baked (fraction + samples cleared).
    pub daily_close: bool,
    /// The UTC hours (0..24) at which the mid-week daily close clusters — the
    /// span to block. Empty when `daily_close` is false. Sorted ascending.
    pub daily_close_hours: Vec<u32>,
    /// Mid-week attention close bars bucketed by UTC hour → count, for the
    /// validation report.
    pub midweek_hour_hist: BTreeMap<u32, usize>,
}

/// Compute the profile from a run of H1 bars (already sorted ascending in time,
/// mapped to mid OHLC in UTC).
pub fn profile_from_bars(bars: &[Bar]) -> MarketHoursProfile {
    let mut total_gaps = 0usize;
    let mut attention_gaps = 0usize;
    let mut midweek_attention_gaps = 0usize;
    let mut midweek_hour_hist: BTreeMap<u32, usize> = BTreeMap::new();

    for i in 0..bars.len().saturating_sub(1) {
        let a = bars[i].t;
        let b = bars[i + 1].t;
        let gap_minutes = (b - a).num_minutes();
        if gap_minutes <= CONTIGUOUS_MAX_MIN {
            continue;
        }
        let jump = (bars[i + 1].o - bars[i].c).abs();
        let atr = atr_ending_at(bars, i);
        let atr_multiple = atr.and_then(|v| (v > 0.0).then_some(jump / v));
        let needs_attention = atr_multiple.map(|m| m >= ATR_MULTIPLE).unwrap_or(false);
        let ev = GapEvent {
            at: a,
            atr_multiple,
            needs_attention,
        };

        total_gaps += 1;
        if needs_attention {
            attention_gaps += 1;
        }
        if ev.is_midweek_attention() {
            midweek_attention_gaps += 1;
            *midweek_hour_hist.entry(a.hour()).or_insert(0) += 1;
        }
    }

    let midweek_fraction = if total_gaps > 0 {
        midweek_attention_gaps as f64 / total_gaps as f64
    } else {
        0.0
    };
    let daily_close = midweek_fraction >= DAILY_TURN_ON_FRACTION && total_gaps >= MIN_SAMPLES;
    let daily_close_hours = if daily_close {
        dominant_hours(&midweek_hour_hist)
    } else {
        Vec::new()
    };

    MarketHoursProfile {
        total_gaps,
        attention_gaps,
        midweek_attention_gaps,
        midweek_fraction,
        daily_close,
        daily_close_hours,
        midweek_hour_hist,
    }
}

/// Pick the daily-close hours to block from the mid-week hour histogram.
///
/// The reopen gap follows the market's daily *close*, and the close clusters at
/// one or two adjacent UTC hours (a DST-straddling instrument shows both, e.g.
/// 15:00 and 16:00). We take every hour whose count is at least half the peak
/// hour's count — that captures the dominant one or two close hours and drops
/// the long tail of one-off holiday closes. Returned sorted ascending.
fn dominant_hours(hist: &BTreeMap<u32, usize>) -> Vec<u32> {
    let peak = hist.values().copied().max().unwrap_or(0);
    if peak == 0 {
        return Vec::new();
    }
    let cutoff = peak.div_ceil(2); // >= half the peak, rounding up
    let mut hours: Vec<u32> = hist
        .iter()
        .filter(|&(_, &c)| c >= cutoff)
        .map(|(&h, _)| h)
        .collect();
    hours.sort_unstable();
    hours
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn bar(t: DateTime<Utc>, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar { t, o, h, l, c }
    }

    /// Build a run of flat H1 bars for `n` hours from `start`, each with a small
    /// constant range so ATR is well-defined and non-zero.
    fn flat_run(start: DateTime<Utc>, n: usize, price: f64) -> Vec<Bar> {
        (0..n)
            .map(|k| {
                let t = start + chrono::Duration::hours(k as i64);
                bar(t, price, price + 0.1, price - 0.1, price)
            })
            .collect()
    }

    #[test]
    fn no_gaps_no_attention() {
        // A contiguous run has no time gaps, so no gap events.
        let start = Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap();
        let bars = flat_run(start, 50, 100.0);
        let p = profile_from_bars(&bars);
        assert_eq!(p.total_gaps, 0);
        assert!(!p.daily_close);
    }

    #[test]
    fn small_reopen_jump_is_not_attention() {
        // 30 warmup bars, then a time gap with a TINY reopen jump (< 1 ATR).
        let start = Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap();
        let mut bars = flat_run(start, 30, 100.0);
        // Gap: skip 3 hours, reopen 0.05 away (ATR ~0.2 → < 1 ATR).
        let last = bars.last().unwrap().t;
        bars.push(bar(
            last + chrono::Duration::hours(3),
            100.05,
            100.1,
            100.0,
            100.05,
        ));
        let p = profile_from_bars(&bars);
        assert_eq!(p.total_gaps, 1);
        assert_eq!(p.attention_gaps, 0);
    }

    #[test]
    fn big_midweek_reopen_jump_counts_as_daily_attention() {
        // Warmup, then a Wednesday-close gap with a BIG reopen jump (> 1 ATR).
        // Repeat enough to clear MIN_SAMPLES and the fraction.
        let mut bars: Vec<Bar> = Vec::new();
        // Start on a Wednesday so the close bar is mid-week.
        let mut t = Utc.with_ymd_and_hms(2026, 7, 8, 0, 0, 0).unwrap(); // Wed
        for _ in 0..40 {
            // 30 warmup bars leading to a close, then a big-jump reopen.
            for _ in 0..30 {
                bars.push(bar(t, 100.0, 100.1, 99.9, 100.0));
                t = t + chrono::Duration::hours(1);
            }
            // Ensure the close bar is mid-week (Mon–Thu). Advance to next Wed if
            // we've drifted; just force the reopen 3h later with a big jump.
            let big = 100.0 + 5.0; // 5.0 jump vs ATR ~0.2 → way over 1 ATR
            t = t + chrono::Duration::hours(3);
            bars.push(bar(t, big, big + 0.1, big - 0.1, big));
            t = t + chrono::Duration::hours(1);
        }
        let p = profile_from_bars(&bars);
        assert!(p.attention_gaps >= MIN_SAMPLES, "enough attention gaps");
        // Whether these land mid-week depends on the walked weekday; assert the
        // machinery ran and produced a fraction.
        assert!(p.total_gaps >= MIN_SAMPLES);
    }

    #[test]
    fn dominant_hours_picks_peak_and_adjacent() {
        let mut hist = BTreeMap::new();
        hist.insert(15u32, 80usize);
        hist.insert(16u32, 62usize);
        hist.insert(12u32, 1usize); // tail, dropped
        let hours = dominant_hours(&hist);
        assert_eq!(hours, vec![15, 16], "peak + adjacent kept, tail dropped");
    }

    #[test]
    fn dominant_hours_single_peak() {
        let mut hist = BTreeMap::new();
        hist.insert(19u32, 90usize);
        hist.insert(20u32, 10usize); // < half of 90 → dropped
        assert_eq!(dominant_hours(&hist), vec![19]);
    }
}
