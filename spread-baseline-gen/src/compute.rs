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
///
/// **Necessary but not sufficient** — an hour must ALSO clear
/// [`PEAK_FRAC`]. See [`profile_for_instrument`] for why both gates are needed.
pub const MED_MULT: f64 = 3.0;

/// An hour is elevated only if its ratio is also at least this fraction of the
/// instrument's PEAK hourly ratio. `0.60` — the spike-dominance gate.
///
/// **Why both gates.** The full 2026-07-13 run exposed a med3 weakness on
/// ~11 TradeNation pairs (USD/CHF, USD/CAD, EUR/JPY, GBP/JPY, USD/ZAR, …) with
/// a *three-tier* spread: a genuine 21:00 UTC (NY-close, 07:00 Brisbane) spike
/// (ratio 2.4–5.5×), a benign "wide" off-session band (0.65–1.8×), and a very
/// tight London/NY-session core (0.22–0.41×). The tight core is the most
/// hours, so it drags the median down until the benign wide band clears
/// `3×median` — re-flagging the 10am–3pm Brisbane block as "rubbish" (the
/// original over-flag bug). AUD/CHF stays clean because its tiers sit closer.
///
/// The peak-fraction gate strips the benign band: the wide band sits at
/// 0.65–1.8× while the spike peak is 2.4–5.5×, so the band is well under 60%
/// of peak and drops out — while the spike (the peak itself) stays. It is
/// self-scaling (a fraction of the instrument's own peak, no absolute floor to
/// calibrate per broker), and it needs the med3 gate as its partner: on a
/// genuinely FLAT instrument (Spot Gold, peak only ~1.25× its median) nothing
/// clears `3×median`, so 60%-of-a-low-peak never mis-fires. Two gates: med3
/// keeps flat instruments empty, peak-frac keeps 3-tier instruments spike-only.
pub const PEAK_FRAC: f64 = 0.60;

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

/// The per-hour percentile used as the *flag* statistic in the minute-based
/// path ([`profile_from_minutes`]). `0.75` — the "bulk of the hour" line that
/// separates a genuine spread hour from a boundary bleed:
/// - A ≤10-minute end-of-hour ramp (the 21:00 spike leaking into hour 20's last
///   minutes) is ~⅙ of the hour, so it sits above p75 and cannot lift it → the
///   bleed hour is NOT flagged.
/// - A real spike filling ≥¼ of the hour (OANDA EUR_USD 21:00 is short but
///   genuine: p75 ratio 1.35× vs 1.24× threshold) DOES lift p75 → flagged.
///
/// p50 (median) was too strict (dropped EUR_USD's short-but-real 21:00 spike);
/// p90 was the original H1-close bleed statistic. Validated on OANDA EUR_USD
/// (short spike, must flag) and GBP_AUD (hour-20 bleed must drop, hour-21 must
/// flag). See the `gbpaud_spread_hour_minute_truth` memory.
pub const FLAG_PERCENTILE: f64 = 0.75;

/// The outcome of reviewing one instrument — an EXPLICIT verdict so a baked
/// `elevated_hours == 0` isn't ambiguous between "analysed, genuinely flat" and
/// "never looked / too little data". Recorded per row so we can see which
/// instruments have actually been analysed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStatus {
    /// Analysed with enough data; the mask (possibly empty) is the verdict.
    /// An empty mask here means "reviewed, genuinely no spread hour".
    Reviewed,
    /// Too few usable bars / hours to compute a trustworthy verdict. The mask
    /// is 0 and the gate should fall back to its NY-close-edge default.
    InsufficientData,
}

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
    /// Per-UTC-hour `p90(spread/mid)` (0.0 for an under-sampled hour). For the
    /// `--dump-hours` calibration report; not baked into the gate table.
    pub hour_p90_frac: [f64; 24],
    /// Per-UTC-hour `ratio(h) = p90_frac / vol` (0.0 for an under-sampled
    /// hour). For the `--dump-hours` calibration report.
    pub hour_ratio: [f64; 24],
    /// Whether this instrument was analysed with enough data. An empty
    /// `elevated_hours` means "genuinely no spread hour" iff `review ==
    /// Reviewed`; with `InsufficientData` it means "couldn't tell".
    pub review: ReviewStatus,
    /// Number of usable bars that contributed.
    pub n_bars: usize,
}

impl SpreadProfile {
    /// The insufficient-data profile — no spread hours because the data was
    /// too thin or volatility degenerate to compute a trustworthy verdict.
    /// Falls the gate back to the NY-close-edge default for this instrument.
    pub fn empty(n_bars: usize) -> Self {
        Self {
            elevated_hours: 0,
            hour_widen_frac: [0.0; 24],
            vol: 0.0,
            median_ratio: 0.0,
            hour_p90_frac: [0.0; 24],
            hour_ratio: [0.0; 24],
            review: ReviewStatus::InsufficientData,
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

    // Per-hour p90 spread-frac; the H1 flag statistic is that same p90 (only
    // hours with enough bars participate). The minute path swaps the flag
    // statistic for the hour's median; both share `apply_gates`.
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

    apply_gates(hour_ratio, hour_p90, vol, n)
}

/// One minute bar reduced to what the minute-aware computation needs: the UTC
/// hour it fell in, its spread fraction, and its mid close (for the hourly
/// volatility resample). Built by the fetchers from each broker's M1 candles.
#[derive(Debug, Clone, Copy)]
pub struct MinuteBar {
    /// UTC timestamp minute-of-day `0..=1439` (`hour*60 + minute`).
    pub utc_minute_of_day: u16,
    /// `(ask_close - bid_close) / mid_close`. Must be finite and ≥ 0.
    pub spread_frac: f64,
    /// Mid close price (> 0), for the hourly-resampled volatility series.
    pub mid_close: f64,
}

impl MinuteBar {
    /// The UTC hour `0..=23` this minute falls in.
    pub fn utc_hour(&self) -> usize {
        (self.utc_minute_of_day / 60) as usize
    }
}

/// Minute-level minimum: an hour needs at least this many *minute* samples
/// before we trust its median/p90. A spread hour lasting the full hour has ~60
/// minutes across many days; a boundary ramp is only ~6/hour/day, so this keeps
/// a thinly-sampled hour out of the flag.
pub const MIN_HOUR_MINUTES: usize = 20;

/// Compute the spread profile from **minute** bars — the bleed-resistant path.
///
/// The H1 [`profile_for_instrument`] samples each hour by its bar *close* spread,
/// so a spike beginning in the last few minutes of the *previous* hour
/// contaminates that hour's bucket and flags a whole hour that is calm for
/// 53/60 minutes (the OANDA GBP_AUD hour-20 / 06:00-Brisbane over-flag). This
/// path instead:
/// - buckets every minute's `spread_frac` by its true UTC hour,
/// - flags an hour on its **p75** minute-ratio (see [`FLAG_PERCENTILE`]): a ≤10-
///   minute end-of-hour ramp sits in the top decile so it can't move the p75 →
///   the bleed hour no longer flags, while a genuine spike that fills ≥¼ of the
///   hour (even a short one like OANDA EUR_USD 21:00) still lifts the p75 over
///   the threshold. The median (p50) was too strict — it dropped EUR_USD's real
///   but short 21:00 spike; p90 was too lenient (the original bleed statistic).
/// - sizes the widen from the **p90** minute-frac (still the spike magnitude).
///
/// `vol` is `median(|Δmid|/mid)` over **hourly-resampled** mids (last minute of
/// each UTC hour) — the SAME scale as the H1 path, so the med3/peak-frac
/// thresholds are unchanged and comparable.
///
/// Bars must be in timestamp order. Same empty-profile guards as the H1 path.
pub fn profile_from_minutes(bars: &[MinuteBar]) -> SpreadProfile {
    let n = bars.len();
    // Minute bars are ~60× denser; require the equivalent of MIN_INSTRUMENT_BARS
    // *hours* of coverage so a thin sample still can't compute a mask.
    if n < MIN_INSTRUMENT_BARS * MIN_HOUR_MINUTES {
        return SpreadProfile::empty(n);
    }

    // Hourly-resampled mids for the volatility baseline: one mid per contiguous
    // UTC-hour run (bars are in timestamp order, so a change in hour-of-day
    // marks a new hour). Consecutive hourly samples give a |Δmid| series on the
    // same scale as the H1 path.
    let mut hourly_mids: Vec<f64> = Vec::new();
    let mut last_hour: Option<usize> = None;
    for b in bars {
        let h = b.utc_hour();
        if last_hour != Some(h) {
            hourly_mids.push(b.mid_close);
            last_hour = Some(h);
        }
    }
    let rets: Vec<f64> = hourly_mids
        .windows(2)
        .filter(|w| w[0] > 0.0)
        .map(|w| (w[1] - w[0]).abs() / w[0])
        .collect();
    let vol = median(&rets);
    if !(vol.is_finite() && vol > 0.0) {
        return SpreadProfile::empty(n);
    }

    // Bucket minute spreads by UTC hour.
    let mut by_hour: [Vec<f64>; 24] = Default::default();
    for b in bars {
        let h = b.utc_hour();
        if h < 24 {
            by_hour[h].push(b.spread_frac);
        }
    }

    // Per-hour flag statistic = p75 minute spread-frac (bulk-of-hour, see
    // FLAG_PERCENTILE), and widen = p90 minute spread-frac (spike magnitude).
    // Only hours with enough minute samples participate.
    let mut hour_flag: [Option<f64>; 24] = [None; 24];
    let mut hour_p90: [f64; 24] = [0.0; 24];
    for h in 0..24 {
        if by_hour[h].len() < MIN_HOUR_MINUTES {
            continue;
        }
        hour_flag[h] = Some(percentile(&by_hour[h], FLAG_PERCENTILE));
        hour_p90[h] = percentile(&by_hour[h], WIDEN_PERCENTILE);
    }

    let flag_ratio: [Option<f64>; 24] = std::array::from_fn(|h| hour_flag[h].map(|m| m / vol));
    apply_gates(flag_ratio, hour_p90, vol, n)
}

/// Shared med3 + peak-fraction gate over per-hour flag ratios and widen p90s.
/// Both the H1 close-based and minute-based paths funnel through this so the
/// elevation logic lives in one place.
///
/// `flag_ratio[h]` = the hour's flag statistic ÷ vol (`None` = under-sampled).
/// `hour_p90[h]` = the hour's p90 spread-frac (the baked widen when elevated).
fn apply_gates(
    flag_ratio: [Option<f64>; 24],
    hour_p90: [f64; 24],
    vol: f64,
    n_bars: usize,
) -> SpreadProfile {
    let ratios: Vec<f64> = flag_ratio.iter().filter_map(|r| *r).collect();
    if ratios.len() < 12 {
        return SpreadProfile::empty(n_bars);
    }
    let median_ratio = median(&ratios);
    if !(median_ratio.is_finite() && median_ratio > 0.0) {
        return SpreadProfile::empty(n_bars);
    }
    let peak_ratio = ratios.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let med_threshold = MED_MULT * median_ratio;
    let peak_threshold = PEAK_FRAC * peak_ratio;

    let mut mask: u32 = 0;
    let mut widen = [0.0_f64; 24];
    let mut ratio_flat = [0.0_f64; 24];
    for h in 0..24 {
        if let Some(r) = flag_ratio[h] {
            ratio_flat[h] = r;
            if r >= med_threshold && r >= peak_threshold {
                mask |= 1 << h;
                widen[h] = hour_p90[h];
            }
        }
    }

    SpreadProfile {
        elevated_hours: mask,
        hour_widen_frac: widen,
        vol,
        median_ratio,
        hour_p90_frac: hour_p90,
        hour_ratio: ratio_flat,
        review: ReviewStatus::Reviewed,
        n_bars,
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

    /// A three-tier series like the over-flagged TN pairs: a tight session
    /// core (most hours), a benign wide off-session band, and one real spike.
    /// `core_frac` < `wide_frac` < `spike_frac`. Wide band = hours 0..=5 and
    /// 22..=23; spike = hour 21; core = everything else.
    fn three_tier_day(days: usize, core: f64, wide: f64, spike: f64) -> Vec<Bar> {
        let mut bars = Vec::new();
        let mut mid = 1.0000_f64;
        for _ in 0..days {
            for h in 0..24u8 {
                mid += if h % 2 == 0 { 0.0002 } else { -0.0002 };
                let frac = if h == 21 {
                    spike
                } else if h <= 5 || h >= 22 {
                    wide
                } else {
                    core
                };
                bars.push(bar(h, frac, mid));
            }
        }
        bars
    }

    #[test]
    fn three_tier_flags_only_the_spike_not_the_wide_band() {
        // USD/CHF-shaped: tight core 0.00009, benign wide band 0.00032,
        // real spike 0.00128. With med3 alone the wide band clears (median is
        // dragged down by the tight core); the peak-fraction gate strips it.
        let bars = three_tier_day(6, 0.00009, 0.00032, 0.00128);
        let p = profile_for_instrument(&bars);
        assert_eq!(
            p.elevated_vec(),
            vec![21],
            "the benign wide off-session band must NOT flag — only the spike \
             (the 12pm-Brisbane-rubbish over-flag fix)"
        );
    }

    #[test]
    fn three_tier_med3_alone_would_over_flag() {
        // Sanity: prove the peak-fraction gate is what's doing the work — the
        // wide band DOES clear 3×median (so med3 alone would flag it).
        let bars = three_tier_day(6, 0.00009, 0.00032, 0.00128);
        let p = profile_for_instrument(&bars);
        let med_threshold = MED_MULT * p.median_ratio;
        // A wide-band hour (e.g. 0) clears 3×median but is < 60% of the peak.
        assert!(
            p.hour_ratio[0] >= med_threshold,
            "wide band should clear 3×median (that's the med3 weakness)"
        );
        assert!(
            p.hour_ratio[0] < PEAK_FRAC * p.hour_ratio[21],
            "wide band must be under 60% of the spike peak (peak-frac excludes it)"
        );
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

    // ---- minute-based path ----

    fn min_bar(hour: u8, minute: u8, spread_frac: f64, mid: f64) -> MinuteBar {
        MinuteBar {
            utc_minute_of_day: hour as u16 * 60 + minute as u16,
            spread_frac,
            mid_close: mid,
        }
    }

    /// `days` days of full 60-min hours at `normal` spread, with a small mid
    /// wobble for vol. `elevated` = closure `(hour, minute) -> Option<frac>`
    /// overriding the spread for specific minutes.
    fn minute_days(
        days: usize,
        normal: f64,
        elevated: impl Fn(u8, u8) -> Option<f64>,
    ) -> Vec<MinuteBar> {
        let mut bars = Vec::new();
        let mut mid = 1.0000_f64;
        for _ in 0..days {
            for h in 0..24u8 {
                mid += if h % 2 == 0 { 0.0002 } else { -0.0002 };
                for m in 0..60u8 {
                    let frac = elevated(h, m).unwrap_or(normal);
                    bars.push(min_bar(h, m, frac, mid));
                }
            }
        }
        bars
    }

    #[test]
    fn minute_end_of_hour_ramp_does_not_flag_the_hour() {
        // Hour 20 is calm for :00–:52 and only ramps in the last ~7 minutes
        // (the 21:00 spike bleeding back — the OANDA GBP_AUD hour-20 case).
        // Hour 21 is a genuine full-hour spike. Only 21 must flag.
        let bars = minute_days(5, 0.0001, |h, m| match (h, m) {
            (20, 53..=59) => Some(0.0010), // 7-min end-of-hour ramp
            (21, _) => Some(0.0010),       // full-hour spike
            _ => None,
        });
        let p = profile_from_minutes(&bars);
        assert_eq!(
            p.elevated_vec(),
            vec![21],
            "an end-of-hour ramp in hour 20 must NOT flag hour 20 (bleed fix); \
             only the full-hour spike (21) flags"
        );
    }

    #[test]
    fn minute_full_hour_spike_flags_and_widen_is_p90_sized() {
        let bars = minute_days(5, 0.0001, |h, _m| (h == 21).then_some(0.0010));
        let p = profile_from_minutes(&bars);
        assert_eq!(p.elevated_vec(), vec![21]);
        // Widen for hour 21 is its p90 minute spread-frac ≈ the spike size.
        assert!(
            p.hour_widen_frac[21] > 0.0009,
            "widen must be p90-sized (spike), got {}",
            p.hour_widen_frac[21]
        );
        assert_eq!(p.hour_widen_frac[20], 0.0);
    }

    #[test]
    fn minute_flat_flags_nothing() {
        let bars = minute_days(5, 0.0001, |_h, _m| None);
        let p = profile_from_minutes(&bars);
        assert_eq!(p.elevated_vec(), Vec::<u8>::new());
    }

    #[test]
    fn minute_thin_data_is_empty() {
        // One hour of minutes only → below the coverage floor → empty.
        let bars = minute_days(1, 0.0001, |_h, _m| None);
        // Truncate to a single hour's worth.
        let bars = &bars[..60];
        let p = profile_from_minutes(bars);
        assert_eq!(p.review, ReviewStatus::InsufficientData);
        assert_eq!(p.elevated_hours, 0);
    }

    #[test]
    fn minute_short_real_spike_flags_but_shorter_bleed_does_not() {
        // The EUR_USD-vs-GBP_AUD calibration in one test: a genuine spike that
        // fills ~20 min of the hour (hour 21, EUR_USD-like short spike) MUST
        // flag; a ~7-min end-of-hour ramp (hour 20 bleed) must NOT. Proves p75
        // is the separating line — p50 would drop the 20-min spike, p90 would
        // keep the 7-min bleed (on H1-close sampling).
        let bars = minute_days(6, 0.0001, |h, m| match (h, m) {
            (21, 40..=59) => Some(0.0015), // 20-min genuine spike
            (20, 53..=59) => Some(0.0015), // 7-min bleed
            _ => None,
        });
        let p = profile_from_minutes(&bars);
        assert_eq!(
            p.elevated_vec(),
            vec![21],
            "a 20-min real spike must flag (p75 lifts) while a 7-min bleed must \
             not (p75 unmoved) — the EUR_USD/GBP_AUD calibration"
        );
    }

    #[test]
    fn minute_half_hour_spike_flags_it() {
        // A spike that fills HALF the hour (30 min) still moves the median over
        // 3× — it's a real (if shorter) spread hour, not a boundary artifact.
        let bars = minute_days(5, 0.0001, |h, m| ((h == 21) && m >= 30).then_some(0.0012));
        let p = profile_from_minutes(&bars);
        // Median of hour 21 = midpoint of 30 calm + 30 spike ≈ boundary; with a
        // 12× spike the median sits at the spike side → flags.
        assert!(
            p.elevated_vec().contains(&21),
            "a half-hour-plus spike should still flag (median crosses 3×)"
        );
    }
}
