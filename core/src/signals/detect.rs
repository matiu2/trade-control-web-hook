//! The five pattern detectors and the per-bar signal geometry that prints when
//! a bar satisfies one — a direct transcription of the Pine detection blocks
//! (`candle-signals-v2.pine` lines ~227-300, 447-498).
//!
//! Each detector is phrased over [`CandleMetrics`] for bar `N` (the bar being
//! evaluated) and, where the pattern spans more than one bar, `N-1` and `N-2`.
//! [`detect_at`] runs all five against a candle slice for the given as-of index
//! and returns the highest-priority [`Detected`] (or `None`).
//!
//! **Confirmed-bar note.** Pine gates tweezer / double-tweezer on
//! `barstate.isconfirmed` so they only print on a closed bar. The engine feeds
//! **only closed candles**, so that gate is always satisfied here and is
//! omitted — every candle in the window is a confirmed bar.

use chrono::{DateTime, Utc};

use super::metrics::CandleMetrics;
use crate::broker::Candle;
use crate::intent::{Direction, SignalKind};

/// `TWEEZER_WICK_THRESHOLD` — the relaxed wick fraction a bar must have to count
/// as the "pinbar-ish" leg of a tweezer (vs the stricter 0.5 for a standalone
/// pinbar). Pine `const float TWEEZER_WICK_THRESHOLD = 0.35`.
const TWEEZER_WICK_THRESHOLD: f64 = 0.35;

/// The geometry latched when a signal prints — the values Pine writes onto the
/// `signal_*` plots. Extremes span the bars the pattern covers (1 for
/// pinbar/floating, 2 for tweezer/regular, 3 for double-tweezer).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalGeometry {
    pub high: f64,
    pub low: f64,
    /// `high - low` for engulfers/tweezers; for multi-bar tweezers Pine instead
    /// sums the component candle ranges into the *size* used for the golden
    /// test — see [`Detected::size`]. `range` here is always the extreme span
    /// (what `signal_range` carries on the wire).
    pub range: f64,
    pub kind: SignalKind,
    /// Open-time of the **earliest** bar the signal covers (Pine
    /// `signal_start_time`): bar `N` for a pinbar, `N-1` for tweezer / regular /
    /// floating, `N-2` for a double-tweezer.
    pub start_time: DateTime<Utc>,
    /// The reversal-close **band anchor** — the price the
    /// `07-close-on-sr-reversal` S/R-band test keys on for this signal. An
    /// engulfer anchors on the **open of its first covered bar** (`N-1`); a
    /// pinbar / tweezer on the print bar's **wick-50%**. Computed here because
    /// the engulfer anchor needs the first bar, which only the detector has in
    /// scope; carried onward on the shell so replay == live. See
    /// [`crate::signals::band_anchor`].
    pub band_anchor: f64,
}

/// A pattern that printed on a bar, with the size used for the golden test.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detected {
    pub direction: Direction,
    pub geometry: SignalGeometry,
    /// The size compared against ATR for the golden flag. For tweezer /
    /// double-tweezer Pine sums the component candle *ranges* (not the extreme
    /// span); for a floating engulfer it is the extreme span; otherwise the
    /// single candle range. Kept separate from [`SignalGeometry::range`] (the
    /// wire `signal_range`) to match Pine exactly.
    pub size: f64,
}

impl Detected {
    /// Golden iff the signal size is at least the ATR at signal time.
    pub fn is_golden(&self, atr: f64) -> bool {
        self.size >= atr
    }
}

/// Per-bar pattern flags for one direction, computed once and combined with
/// priority. Mirrors the `bullish_*` / `bearish_*` booleans in the Pine.
struct DirFlags {
    double_tweezer: bool,
    tweezer: bool,
    pinbar: bool,
    regular: bool,
    floating: bool,
}

impl DirFlags {
    /// Any pattern at all printed this bar.
    fn any(&self) -> bool {
        self.double_tweezer || self.tweezer || self.pinbar || self.regular || self.floating
    }
}

/// All five detectors as configured. `similarity_pct` is the tweezer
/// high/low similarity threshold (Pine input, default 20.0).
#[derive(Debug, Clone, Copy)]
pub struct DetectFlags {
    pub pinbars: bool,
    pub tweezers: bool,
    pub double_tweezers: bool,
    pub regular_engulfers: bool,
    pub floating_engulfers: bool,
    pub similarity_pct: f64,
}

impl Default for DetectFlags {
    fn default() -> Self {
        Self {
            pinbars: true,
            tweezers: true,
            double_tweezers: true,
            regular_engulfers: true,
            floating_engulfers: true,
            similarity_pct: 20.0,
        }
    }
}

/// Detect the highest-priority signal that prints on `candles[i]`, if any.
///
/// `candles` must be ascending; `i` is the index of the bar being evaluated.
/// Returns `None` if `i` is out of range, the bar is invalid, or no enabled
/// pattern fires. Direction is implied by the pattern (bullish vs bearish);
/// when both somehow fire (they are mutually exclusive in practice) the bullish
/// side wins, matching the Pine evaluation order.
pub fn detect_at(candles: &[Candle], i: usize, flags: &DetectFlags) -> Option<Detected> {
    let cur = candles.get(i)?;
    let m = CandleMetrics::of(cur);
    if !m.valid_candle {
        return None;
    }
    let prev = i.checked_sub(1).and_then(|j| candles.get(j));
    let prev2 = i.checked_sub(2).and_then(|j| candles.get(j));
    let pm = prev.map(CandleMetrics::of);
    let pm2 = prev2.map(CandleMetrics::of);

    let bull = dir_flags(Direction::Long, &m, pm.as_ref(), pm2.as_ref(), flags);
    let bear = dir_flags(Direction::Short, &m, pm.as_ref(), pm2.as_ref(), flags);

    if bull.any() {
        return Some(build(
            Direction::Long,
            &bull,
            cur,
            prev,
            prev2,
            &m,
            pm.as_ref(),
            pm2.as_ref(),
        ));
    }
    if bear.any() {
        return Some(build(
            Direction::Short,
            &bear,
            cur,
            prev,
            prev2,
            &m,
            pm.as_ref(),
            pm2.as_ref(),
        ));
    }
    None
}

/// Compute the five pattern flags for one direction.
fn dir_flags(
    dir: Direction,
    m: &CandleMetrics,
    pm: Option<&CandleMetrics>,
    pm2: Option<&CandleMetrics>,
    flags: &DetectFlags,
) -> DirFlags {
    let pinbar = flags.pinbars && pinbar(dir, m, pm);
    let (double_tweezer, tweezer) = tweezers(dir, m, pm, pm2, flags);
    let regular = flags.regular_engulfers && regular_engulfer(dir, m, pm);
    let floating = flags.floating_engulfers && floating_engulfer(dir, m, pm);
    DirFlags {
        // Priority: double_tweezer shadows tweezer shadows pinbar (the `and not`
        // chains in Pine). Pinbar only counts when neither tweezer kind fires.
        double_tweezer,
        tweezer,
        pinbar: pinbar && !tweezer && !double_tweezer,
        regular,
        floating,
    }
}

/// Standalone pinbar: the strict 0.5-wick test plus the breakout requirement
/// (`low < low[1]` bullish / `high > high[1]` bearish). Requires a prior bar —
/// with none, Pine's `low[1]` is `na` and the comparison is false.
fn pinbar(dir: Direction, m: &CandleMetrics, pm: Option<&CandleMetrics>) -> bool {
    let Some(pm) = pm else { return false };
    match dir {
        Direction::Long => {
            m.body_top >= m.top_25
                && m.body_bottom >= m.midpoint
                && m.lower_wick >= m.range * 0.5
                && m.low < pm.low
        }
        Direction::Short => {
            m.body_bottom <= m.bottom_25
                && m.body_top <= m.midpoint
                && m.upper_wick >= m.range * 0.5
                && m.high > pm.high
        }
    }
}

/// The "pinbar-ish" leg test used by tweezers — the relaxed 0.35-wick threshold,
/// no breakout requirement. Mirrors `curr_bullish_pinbar_tweezer` etc.
fn pinbar_tweezer_leg(dir: Direction, m: &CandleMetrics) -> bool {
    if !m.valid_candle {
        return false;
    }
    match dir {
        Direction::Long => {
            m.body_top >= m.top_25
                && m.body_bottom >= m.midpoint
                && m.lower_wick >= m.range * TWEEZER_WICK_THRESHOLD
        }
        Direction::Short => {
            m.body_bottom <= m.bottom_25
                && m.body_top <= m.midpoint
                && m.upper_wick >= m.range * TWEEZER_WICK_THRESHOLD
        }
    }
}

/// `(double_tweezer, tweezer)` for the direction. Double-tweezer shadows plain
/// tweezer (Pine `and not bullish_double_tweezer`).
fn tweezers(
    dir: Direction,
    m: &CandleMetrics,
    pm: Option<&CandleMetrics>,
    pm2: Option<&CandleMetrics>,
    flags: &DetectFlags,
) -> (bool, bool) {
    let Some(pm) = pm else {
        return (false, false);
    };
    let cur_leg = pinbar_tweezer_leg(dir, m);
    let prev_leg = pinbar_tweezer_leg(dir, pm);

    // Pair similarity (highs and lows of N, N-1 within similarity_pct of the
    // larger range).
    let pair_range = m.range.max(pm.range);
    let has_pair = pair_range > 0.0;
    let pair_thresh = pair_range * flags.similarity_pct * 0.01;
    let highs_close = has_pair && (m.high - pm.high).abs() <= pair_thresh;
    let lows_close = has_pair && (m.low - pm.low).abs() <= pair_thresh;
    let valid_pair = has_pair && pm.valid_candle && highs_close && lows_close;

    // Triple similarity (N, N-1, N-2) for the double tweezer.
    let double = match pm2 {
        Some(pm2) => {
            let prev2_leg = pinbar_tweezer_leg(dir, pm2);
            let triple_range = m.range.max(pm.range).max(pm2.range);
            let has_triple = triple_range > 0.0;
            let triple_thresh = triple_range * flags.similarity_pct * 0.01;
            let triple_highs = has_triple
                && (m.high - pm.high).abs() <= triple_thresh
                && (pm.high - pm2.high).abs() <= triple_thresh;
            let triple_lows = has_triple
                && (m.low - pm.low).abs() <= triple_thresh
                && (pm.low - pm2.low).abs() <= triple_thresh;
            let valid_triple =
                has_triple && pm.valid_candle && pm2.valid_candle && triple_highs && triple_lows;
            flags.double_tweezers && valid_triple && cur_leg && prev_leg && prev2_leg
        }
        None => false,
    };

    let tweezer = flags.tweezers && valid_pair && cur_leg && prev_leg && !double;
    (double, tweezer)
}

/// Regular engulfer: prior opposite-colour bar fully engulfed with a close
/// beyond the prior high/low and the close in the extreme quartile. Requires a
/// prior bar.
fn regular_engulfer(dir: Direction, m: &CandleMetrics, pm: Option<&CandleMetrics>) -> bool {
    let Some(pm) = pm else { return false };
    if !pm.valid_candle {
        return false;
    }
    match dir {
        Direction::Long => {
            pm.is_bearish
                && m.is_bullish
                && m.open <= pm.close
                && m.close > pm.high
                && m.close_in_top_25()
        }
        Direction::Short => {
            pm.is_bullish
                && m.is_bearish
                && m.open >= pm.close
                && m.close < pm.low
                && m.close_in_bottom_25()
        }
    }
}

/// Floating engulfer: same-colour prior bar, close beyond the prior high/low,
/// close in the extreme quartile (no body-open constraint). Requires a prior
/// bar.
fn floating_engulfer(dir: Direction, m: &CandleMetrics, pm: Option<&CandleMetrics>) -> bool {
    let Some(pm) = pm else { return false };
    if !pm.valid_candle {
        return false;
    }
    match dir {
        Direction::Long => {
            pm.is_bullish && m.is_bullish && m.close > pm.high && m.close_in_top_25()
        }
        Direction::Short => {
            pm.is_bearish && m.is_bearish && m.close < pm.low && m.close_in_bottom_25()
        }
    }
}

/// Build the [`Detected`] for the winning direction, computing the signal
/// extremes / size / kind / start-time per the priority order — mirroring the
/// Pine `bull_sig_*` / `bear_sig_*` precompute and the `f_*_kind()` helpers.
#[allow(clippy::too_many_arguments)]
fn build(
    dir: Direction,
    f: &DirFlags,
    cur: &Candle,
    prev: Option<&Candle>,
    prev2: Option<&Candle>,
    m: &CandleMetrics,
    pm: Option<&CandleMetrics>,
    pm2: Option<&CandleMetrics>,
) -> Detected {
    // `nz(high[1], high)`: fall back to the current bar's value when the prior
    // bar is absent (start of history).
    let ph = pm.map_or(m.high, |x| x.high);
    let pl = pm.map_or(m.low, |x| x.low);
    let p2h = pm2.map_or(m.high, |x| x.high);
    let p2l = pm2.map_or(m.low, |x| x.low);
    let prev_range = pm.map_or(0.0, |x| x.range);
    let prev2_range = pm2.map_or(0.0, |x| x.range);

    let (high, low, size, kind) = if f.double_tweezer {
        (
            m.high.max(ph).max(p2h),
            m.low.min(pl).min(p2l),
            m.range + prev_range + prev2_range,
            SignalKind::DoubleTweezer,
        )
    } else if f.tweezer {
        (
            m.high.max(ph),
            m.low.min(pl),
            m.range + prev_range,
            SignalKind::Tweezer,
        )
    } else if f.floating {
        // Floating engulfer: extreme span over the current bar only (Pine's
        // bull_sig_high/low are `high`/`low` for non-tweezer), but the *size*
        // is the extreme span (high - low). Kind = floating.
        (m.high, m.low, m.high - m.low, SignalKind::FloatingEngulfer)
    } else if f.regular {
        (m.high, m.low, m.range, SignalKind::RegularEngulfer)
    } else {
        // Pinbar.
        (m.high, m.low, m.range, SignalKind::Pinbar)
    };

    // signal_start_time: earliest covered bar. double → N-2, the 2-bar kinds
    // (tweezer / regular / floating) → N-1, pinbar → N.
    let start_time = if f.double_tweezer {
        prev2.map_or(cur.time, |c| c.time)
    } else if f.tweezer || f.regular || f.floating {
        prev.map_or(cur.time, |c| c.time)
    } else {
        cur.time
    };

    // Band anchor origin: for a regular/floating engulfer the anchor is the
    // FIRST bar's open (`N-1`), so the origin is `prev` (falling back to `cur` at
    // the start of history). Pinbars key off the print bar's wick, so origin ==
    // print; tweezers ignore the origin (wick pattern) — pass `cur` for both.
    let origin = if (kind == SignalKind::RegularEngulfer || kind == SignalKind::FloatingEngulfer)
        && let Some(p) = prev
    {
        p
    } else {
        cur
    };
    let band_anchor = crate::signals::band_anchor(kind, dir, origin, cur);

    Detected {
        direction: dir,
        geometry: SignalGeometry {
            high,
            low,
            range: high - low,
            kind,
            start_time,
            band_anchor,
        },
        size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn k(time: &str, o: f64, h: f64, l: f64, c: f64) -> Candle {
        Candle {
            time: ts(time),
            o,
            h,
            l,
            c,
        }
    }

    fn flags() -> DetectFlags {
        DetectFlags::default()
    }

    // A bullish (hammer-style) pinbar: small body high in the range, long lower
    // wick ≥ 50% of range, and low below the prior bar's low.
    #[test]
    fn bullish_pinbar_detected() {
        let candles = [
            // prior bar — its low is 1.05, the pinbar must dip below it.
            k("2026-06-16T10:00:00Z", 1.10, 1.12, 1.05, 1.06),
            // pinbar: range 1.00..1.20 = 0.20. body 1.16..1.18 (top quartile),
            // lower wick = body_bottom - low = 1.16 - 1.00 = 0.16 ≥ 0.10. low
            // 1.00 < prior low 1.05.
            k("2026-06-16T11:00:00Z", 1.16, 1.20, 1.00, 1.18),
        ];
        let d = detect_at(&candles, 1, &flags()).expect("pinbar");
        assert_eq!(d.direction, Direction::Long);
        assert_eq!(d.geometry.kind, SignalKind::Pinbar);
        // single-bar geometry: extremes are the pinbar's own.
        assert!((d.geometry.high - 1.20).abs() < 1e-12);
        assert!((d.geometry.low - 1.00).abs() < 1e-12);
        assert_eq!(d.geometry.start_time, ts("2026-06-16T11:00:00Z"));
    }

    #[test]
    fn bearish_regular_engulfer_detected() {
        let candles = [
            // prior bullish bar, range 1.00..1.10, close 1.08.
            k("2026-06-16T10:00:00Z", 1.02, 1.10, 1.00, 1.08),
            // bearish engulfer: open >= prior close (1.08), close < prior low
            // (1.00), close in bottom quartile of its own range.
            // range 0.90..1.09 = 0.19; close 0.91 → pos (0.91-0.90)/0.19 ≈ 0.05.
            k("2026-06-16T11:00:00Z", 1.08, 1.09, 0.90, 0.91),
        ];
        let d = detect_at(&candles, 1, &flags()).expect("engulfer");
        assert_eq!(d.direction, Direction::Short);
        assert_eq!(d.geometry.kind, SignalKind::RegularEngulfer);
        // 2-bar kind → start_time is the prior bar.
        assert_eq!(d.geometry.start_time, ts("2026-06-16T10:00:00Z"));
    }

    #[test]
    fn tweezer_spans_two_bars() {
        // Two near-identical bullish pinbar-ish bars (highs/lows within 20% of
        // the larger range) → tweezer; extremes span both, start = N-1.
        let candles = [
            k("2026-06-16T09:00:00Z", 1.10, 1.12, 1.00, 1.05), // context
            // N-1: lower wick ≥ 0.35*range, body high. range 1.00..1.20=0.20.
            k("2026-06-16T10:00:00Z", 1.15, 1.20, 1.00, 1.17),
            // N: highs/lows close to N-1.
            k("2026-06-16T11:00:00Z", 1.16, 1.205, 0.99, 1.18),
        ];
        let d = detect_at(&candles, 2, &flags()).expect("tweezer");
        assert_eq!(d.geometry.kind, SignalKind::Tweezer);
        assert!((d.geometry.high - 1.205).abs() < 1e-12, "span high");
        assert!((d.geometry.low - 0.99).abs() < 1e-12, "span low");
        assert_eq!(d.geometry.start_time, ts("2026-06-16T10:00:00Z"));
    }

    #[test]
    fn no_signal_on_doji() {
        let candles = [
            k("2026-06-16T10:00:00Z", 1.10, 1.12, 1.05, 1.06),
            k("2026-06-16T11:00:00Z", 1.10, 1.20, 1.00, 1.10), // body 0 → invalid
        ];
        assert!(detect_at(&candles, 1, &flags()).is_none());
    }

    #[test]
    fn disabled_pattern_not_detected() {
        let candles = [
            k("2026-06-16T10:00:00Z", 1.10, 1.12, 1.05, 1.06),
            k("2026-06-16T11:00:00Z", 1.16, 1.20, 1.00, 1.18),
        ];
        let mut f = flags();
        f.pinbars = false;
        assert!(detect_at(&candles, 1, &f).is_none());
    }

    #[test]
    fn golden_compares_size_to_atr() {
        let candles = [
            k("2026-06-16T10:00:00Z", 1.10, 1.12, 1.05, 1.06),
            k("2026-06-16T11:00:00Z", 1.16, 1.20, 1.00, 1.18),
        ];
        let d = detect_at(&candles, 1, &flags()).unwrap();
        // size = range = 0.20.
        assert!(d.is_golden(0.10));
        assert!(!d.is_golden(0.50));
    }
}
