//! Pure spread-hour computation — no I/O.
//!
//! Takes a per-instrument series of H1 bid/ask bars and derives the
//! spread-hour profile using the **med3** rule: an hour is a spread hour iff
//! its `spread/volatility` ratio is at least 3× the instrument's *own* median
//! hourly ratio.
//!
//! This is the vol-relative recalibration of the older scale-blind
//! `median≥1.5×quiet OR p90≥3×quiet` test (see the
//! `spread_hour_mask_vol_relative_recalibration` memory). The threshold is
//! measured against volatility rather than the instrument's quiet floor, so a
//! tight cross (AUD/CHF ~0.9p quiet) no longer flags its ordinary ~2.2p
//! overnight band as "rubbish" — only the genuine 12p liquidity-vacuum spike
//! hour trips it.
//!
//! Broker-agnostic by construction: `ratio(h) = p90(spread/mid, h) / vol` is
//! dimensionless and relative to the instrument's own median, so it transfers
//! across brokers whose absolute spread magnitudes differ (OANDA EUR/USD peak
//! 1.81× vs TradeNation 5.58× — same 21:00 UTC hour, different magnitude). An
//! *absolute* ratio cutoff would NOT transfer; med3 does.

/// One H1 bar reduced to what the computation needs: the UTC hour it opened,
/// its closing spread fraction (`(ask-bid)/mid`), and its mid close (for the
/// consecutive-bar volatility series). Built by the fetchers from each
/// broker's native candle type.
#[derive(Debug, Clone, Copy)]
pub struct Bar {
    /// UTC hour-of-day `0..=23` the bar opened in.
    pub utc_hour: u8,
    /// `(ask_close - bid_close) / mid_close`. Must be finite and ≥ 0.
    pub spread_frac: f64,
    /// Mid close price (> 0), for the `|Δmid|/mid` volatility series.
    pub mid_close: f64,
}

/// An hour is elevated iff its ratio is at least this multiple of the
/// instrument's own median hourly ratio. `3×` — validated across the full
/// sampler + OANDA-candle audit (normal hours 0.5–1.7×, elevated-but-fine
/// band ~3.7×, genuine spikes 5.6–20×). See the memory.
pub const MED_MULT: f64 = 3.0;

/// An hour needs at least this many bars before we trust its p90 spread —
/// fewer and a 1–2-bar bucket could declare a spread hour on noise.
pub const MIN_HOUR_BARS: usize = 3;

/// An instrument needs at least this many usable bars overall before we
/// compute a mask at all — below this the per-hour buckets and the vol
/// baseline are too thin to trust. Low-confidence ⇒ mask 0 (fallback).
pub const MIN_INSTRUMENT_BARS: usize = 48;

/// The per-hour spread we bake as the System-2 stop-widen amount when that
/// hour is elevated: the hour's p90 spread *fraction* (robust to a lone freak
/// print, which `max` would chase).
pub const WIDEN_PERCENTILE: f64 = 0.90;

/// The computed spread profile for one (broker, instrument).
#[derive(Debug, Clone, PartialEq)]
pub struct SpreadProfile {
    /// Bit `h` set ⇒ UTC hour `h` is a spread hour for this instrument.
    pub elevated_hours: u32,
    /// Per-UTC-hour widen size as a **spread fraction** (that hour's p90
    /// `spread/mid` when elevated, else 0.0). Indexed by hour `0..24`.
    pub hour_widen_frac: [f64; 24],
    /// Baseline volatility `median(|Δmid|/mid)` — for the validation report.
    pub vol: f64,
    /// Median over hours of `ratio(h)` — the med3 denominator; for the report.
    pub median_ratio: f64,
    /// Number of usable bars that contributed.
    pub n_bars: usize,
}

impl SpreadProfile {
    /// The empty profile — no spread hours, used when data is too thin or
    /// volatility is degenerate. Falls the gate back to the NY-close-edge
    /// default for this instrument.
    pub fn empty(n_bars: usize) -> Self {
        Self {
            elevated_hours: 0,
            hour_widen_frac: [0.0; 24],
            vol: 0.0,
            median_ratio: 0.0,
            n_bars,
        }
    }

    /// The elevated hours as a sorted `Vec<u8>` (for reports and tests).
    pub fn elevated_vec(&self) -> Vec<u8> {
        (0..24u8)
            .filter(|h| self.elevated_hours & (1 << h) != 0)
            .collect()
    }
}

/// The `p`-th percentile of a slice (linear interpolation). `p` in `[0,1]`.
/// Sorts a copy; empty ⇒ `NaN`. Caller gates on non-empty via `MIN_HOUR_BARS`.
fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let k = (v.len() - 1) as f64 * p;
    let lo = k.floor() as usize;
    let hi = (lo + 1).min(v.len() - 1);
    let frac = k - lo as f64;
    v[lo] + (v[hi] - v[lo]) * frac
}

/// Median of a slice. Empty ⇒ `NaN`.
fn median(values: &[f64]) -> f64 {
    percentile(values, 0.5)
}

/// Compute the med3 spread profile for one instrument from its H1 bars.
///
/// Guards (each ⇒ empty profile):
/// - fewer than [`MIN_INSTRUMENT_BARS`] usable bars,
/// - `vol == 0` or non-finite (structurally impossible with real candles, but
///   kept for safety — the sampler's degenerate case),
/// - fewer than a handful of populated hours (no meaningful median-of-ratios).
///
/// Bars are assumed already filtered to finite, positive-mid, ≥0-spread rows
/// **in timestamp order** (the `|Δmid|` series reads consecutive bars).
pub fn profile_for_instrument(bars: &[Bar]) -> SpreadProfile {
    let n = bars.len();
    if n < MIN_INSTRUMENT_BARS {
        return SpreadProfile::empty(n);
    }

    // Baseline volatility from consecutive mid closes.
    let rets: Vec<f64> = bars
        .windows(2)
        .filter(|w| w[0].mid_close > 0.0)
        .map(|w| (w[1].mid_close - w[0].mid_close).abs() / w[0].mid_close)
        .collect();
    let vol = median(&rets);
    if !(vol.is_finite() && vol > 0.0) {
        return SpreadProfile::empty(n);
    }

    // Bucket spread fractions by UTC hour.
    let mut by_hour: [Vec<f64>; 24] = Default::default();
    for b in bars {
        if (b.utc_hour as usize) < 24 {
            by_hour[b.utc_hour as usize].push(b.spread_frac);
        }
    }

    // Per-hour p90 spread-frac and ratio (only hours with enough bars).
    let mut hour_p90: [f64; 24] = [0.0; 24];
    let mut hour_ratio: [Option<f64>; 24] = [None; 24];
    for h in 0..24 {
        if by_hour[h].len() < MIN_HOUR_BARS {
            continue;
        }
        let p90 = percentile(&by_hour[h], WIDEN_PERCENTILE);
        hour_p90[h] = p90;
        hour_ratio[h] = Some(p90 / vol);
    }

    let ratios: Vec<f64> = hour_ratio.iter().filter_map(|r| *r).collect();
    // Need a spread of populated hours to have a meaningful median-of-ratios.
    if ratios.len() < 12 {
        return SpreadProfile::empty(n);
    }
    let median_ratio = median(&ratios);
    if !(median_ratio.is_finite() && median_ratio > 0.0) {
        return SpreadProfile::empty(n);
    }

    let threshold = MED_MULT * median_ratio;
    let mut mask: u32 = 0;
    let mut widen = [0.0_f64; 24];
    for h in 0..24 {
        if let Some(r) = hour_ratio[h]
            && r >= threshold
        {
            mask |= 1 << h;
            widen[h] = hour_p90[h];
        }
    }

    SpreadProfile {
        elevated_hours: mask,
        hour_widen_frac: widen,
        vol,
        median_ratio,
        n_bars: n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a bar with a fixed mid and a spread fraction, at a UTC hour.
    fn bar(hour: u8, spread_frac: f64, mid: f64) -> Bar {
        Bar {
            utc_hour: hour,
            spread_frac,
            mid_close: mid,
        }
    }

    /// A full-day series: every hour gets `per_hour` bars at a normal spread,
    /// with a little mid movement so vol > 0. `spike_hour` gets a wide spread.
    fn synthetic_day(days: usize, normal_frac: f64, spike_hour: Option<(u8, f64)>) -> Vec<Bar> {
        let mut bars = Vec::new();
        let mut mid = 1.0000_f64;
        for _ in 0..days {
            for h in 0..24u8 {
                // Small deterministic mid wobble → non-zero vol.
                mid += if h % 2 == 0 { 0.0002 } else { -0.0002 };
                let frac = match spike_hour {
                    Some((sh, sf)) if sh == h => sf,
                    _ => normal_frac,
                };
                bars.push(bar(h, frac, mid));
            }
        }
        bars
    }

    #[test]
    fn too_few_bars_is_empty() {
        let bars = synthetic_day(1, 0.0001, None); // 24 bars < MIN_INSTRUMENT_BARS
        let p = profile_for_instrument(&bars);
        assert_eq!(p.elevated_hours, 0);
        assert_eq!(p.n_bars, 24);
    }

    #[test]
    fn flat_spread_flags_nothing() {
        // Every hour identical spread → every ratio equal → none ≥ 3× median.
        let bars = synthetic_day(5, 0.0001, None);
        let p = profile_for_instrument(&bars);
        assert_eq!(
            p.elevated_vec(),
            Vec::<u8>::new(),
            "flat instrument (Gold-like) must flag no spread hours"
        );
    }

    #[test]
    fn single_spike_hour_flags_only_that_hour() {
        // One hour at 10× the normal spread → its ratio ≫ 3× median.
        let bars = synthetic_day(5, 0.0001, Some((21, 0.0010)));
        let p = profile_for_instrument(&bars);
        assert_eq!(
            p.elevated_vec(),
            vec![21],
            "only the 10x spike hour should flag (EUR/USD 21:00-like)"
        );
        // The baked widen for hour 21 is its p90 spread fraction (~the spike).
        assert!(p.hour_widen_frac[21] > 0.0009);
        assert_eq!(p.hour_widen_frac[20], 0.0);
    }

    #[test]
    fn zero_vol_is_empty() {
        // All mids identical → vol == 0 → guard → empty (the degenerate case).
        let mut bars = Vec::new();
        for _ in 0..3 {
            for h in 0..24u8 {
                bars.push(bar(h, 0.0001, 1.0)); // constant mid
            }
        }
        let p = profile_for_instrument(&bars);
        assert_eq!(p.elevated_hours, 0);
        assert_eq!(p.vol, 0.0);
    }

    #[test]
    fn mild_elevation_below_3x_not_flagged() {
        // One hour at ~2× the normal spread — elevated but below the 3× cut.
        let bars = synthetic_day(5, 0.0001, Some((21, 0.00018)));
        let p = profile_for_instrument(&bars);
        assert_eq!(
            p.elevated_vec(),
            Vec::<u8>::new(),
            "a mild 1.8x band must NOT flag (the AUD/CHF 2.2p overnight fix)"
        );
    }

    #[test]
    fn percentile_interpolates() {
        let v = vec![0.0, 1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&v, 0.0), 0.0);
        assert_eq!(percentile(&v, 1.0), 4.0);
        assert_eq!(percentile(&v, 0.5), 2.0);
    }
}
