//! The recompute-from-window signal state machine — the port of the Pine
//! per-bar update loops plus the latch/confirm logic
//! (`candle-signals-v2.pine` lines ~311-498, 555-564).
//!
//! # What it computes
//!
//! Pine tracks every printed signal in arrays and, each bar, runs a state
//! machine (pending → valid → invalid) over them while latching the
//! most-recent signal's geometry onto the `signal_*` plots. The "Long/Short
//! Pattern" alert fires when a new signal prints **or** the latched signal just
//! validated. The worker reads the latched geometry + `signal_confirmed` to
//! resolve the enter.
//!
//! Because the engine is stateless across cron ticks (Stage-E decision), this
//! replays a back-window of closed candles from scratch each tick:
//! [`latched_signal_at`] runs bars `0..=as_of` and returns the latch as of the
//! `as_of` bar, together with whether the alert **would fire on that bar**
//! (`fires`). The engine fires the `PinePattern` enter exactly when `fires` is
//! true and the latched direction matches the plan.
//!
//! # Confirmation timing — fixes Pine bug #10B
//!
//! Pine can latch `signal_confirmed = 1` off a bar that hasn't closed (the
//! ADIDAS 5:30-vs-5:45 case). The engine only sees **closed** candles, so a
//! confirming push here is always a closed bar — the port confirms only on
//! closed pushing bars within `confirm_bars`, which is the correct behaviour.
//!
//! # Opposing-signal invalidation (golden-protect)
//!
//! A pending-or-recently-valid signal inside its confirm window is invalidated
//! if an opposite-direction signal prints, **unless** the pending signal is
//! golden and the opposer is not. This can flip a previously-valid signal back
//! to invalid. Ported faithfully.

use chrono::{DateTime, Utc};

use super::atr::{atr_length_for, wilder_atr};
use super::detect::{DetectFlags, Detected, detect_at};
use crate::broker::{Candle, Granularity};
use crate::intent::{Direction, SignalKind};

/// The signal detector's configuration — the Pine `input.*` knobs the engine
/// needs to reproduce a chart's behaviour. The chart's study ships
/// [`Self::pine_defaults`]; a future plan field could override these.
#[derive(Debug, Clone, Copy)]
pub struct DetectorConfig {
    pub detect: DetectFlags,
    /// Bars after a signal prints within which a confirming push validates it.
    pub confirm_bars: usize,
    /// Window of prior bars whose extreme is reported as `recent_high` /
    /// `recent_low` (SL anchor).
    pub sl_lookback: usize,
    /// The timeframe — drives the ATR length.
    pub granularity: Granularity,
}

impl DetectorConfig {
    /// The defaults baked into `candle-signals-v2.pine` (`confirm_bars = 2`,
    /// `sl_lookback = 5`, `similarity_pct = 20`, all five patterns on).
    pub fn pine_defaults(granularity: Granularity) -> Self {
        Self {
            detect: DetectFlags::default(),
            confirm_bars: 2,
            sl_lookback: 5,
            granularity,
        }
    }
}

/// The latched signal as of a bar — the value the Pine `signal_*` plots hold,
/// plus whether the pattern alert would fire on that bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatchedSignal {
    pub direction: Direction,
    pub kind: SignalKind,
    pub signal_high: f64,
    pub signal_low: f64,
    pub signal_range: f64,
    pub signal_start_time: DateTime<Utc>,
    pub golden: bool,
    pub signal_confirmed: bool,
    /// ATR at the as-of bar (the value Pine latches as `atr`).
    pub atr: Option<f64>,
    /// Highest high over `sl_lookback` bars strictly preceding the signal bar.
    pub recent_high: Option<f64>,
    /// Lowest low over the same window.
    pub recent_low: Option<f64>,
    /// True iff the pattern alert (`*_signal or *_just_valid`) fires on the
    /// as-of bar — a new signal printed, or the latched signal just validated.
    pub fires: bool,
}

/// One tracked signal in the state machine. Mirrors the per-direction Pine
/// arrays, collapsed into a struct.
#[derive(Debug, Clone, Copy)]
struct Tracked {
    direction: Direction,
    high: f64,
    low: f64,
    /// Bar index the signal printed on.
    signal_bar: usize,
    state: SigState,
    golden: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SigState {
    Pending,
    Valid,
    Invalid,
}

/// The latch carried across bars (Pine `signal_*_v`).
#[derive(Debug, Clone, Copy)]
struct Latch {
    direction: Direction,
    kind: SignalKind,
    high: f64,
    low: f64,
    range: f64,
    start_time: DateTime<Utc>,
    golden: bool,
    confirmed: bool,
    atr: Option<f64>,
}

/// Compute the latched signal as of `candles[as_of]` by replaying the per-bar
/// state machine over `candles[0..=as_of]`.
///
/// `candles` must be ascending closed candles. Returns `None` if no signal has
/// printed by `as_of`, or `as_of` is out of range. The window should contain at
/// least `confirm_bars + 1` bars after the signal for confirmation to resolve,
/// plus the pattern depth (3) and the `sl_lookback` ahead of the signal — the
/// caller sizes the back-window accordingly.
pub fn latched_signal_at(
    candles: &[Candle],
    as_of: usize,
    cfg: &DetectorConfig,
) -> Option<LatchedSignal> {
    if as_of >= candles.len() {
        return None;
    }
    let atr_len = atr_length_for(cfg.granularity);
    let mut tracked: Vec<Tracked> = Vec::new();
    let mut latch: Option<Latch> = None;

    let mut fired_on_as_of = false;
    let mut latch_signal_bar: Option<usize> = None;

    for bar in 0..=as_of {
        let mut just_valid = false;

        // ---- 1. Update existing tracked signals against this bar. ----
        // Detect whether an opposing signal prints this bar (needed by the
        // in-window invalidation rule) — peek the detection once.
        let printed = detect_at(candles, bar, &cfg.detect);
        update_tracked(
            &mut tracked,
            bar,
            candles,
            cfg,
            printed.as_ref(),
            &mut latch,
            latch_signal_bar,
            &mut just_valid,
        );

        // ---- 2. Capture a new signal printing this bar; overwrite the latch. ----
        if let Some(d) = printed {
            let atr = wilder_atr(&candles[..=bar], atr_len);
            let golden = atr.is_some_and(|a| d.is_golden(a));
            tracked.push(Tracked {
                direction: d.direction,
                high: d.geometry.high,
                low: d.geometry.low,
                signal_bar: bar,
                state: SigState::Pending,
                golden,
            });
            latch = Some(Latch {
                direction: d.direction,
                kind: d.geometry.kind,
                high: d.geometry.high,
                low: d.geometry.low,
                range: d.geometry.range,
                start_time: d.geometry.start_time,
                golden,
                confirmed: false,
                atr,
            });
            latch_signal_bar = Some(bar);
            if bar == as_of {
                fired_on_as_of = true; // a fresh signal fires the alert.
            }
        }

        // The alert also fires on the bar a latched signal *just* validated.
        if bar == as_of && just_valid {
            fired_on_as_of = true;
        }
    }

    let latch = latch?;
    let signal_bar = latch_signal_bar?;
    let (recent_high, recent_low) = recent_extremes(candles, signal_bar, cfg.sl_lookback);
    Some(LatchedSignal {
        direction: latch.direction,
        kind: latch.kind,
        signal_high: latch.high,
        signal_low: latch.low,
        signal_range: latch.range,
        signal_start_time: latch.start_time,
        golden: latch.golden,
        signal_confirmed: latch.confirmed,
        atr: latch.atr,
        recent_high,
        recent_low,
        fires: fired_on_as_of,
    })
}

/// Run the per-bar update over the currently-tracked signals — the bullish and
/// bearish Pine update loops, merged (each tracked signal carries its own
/// direction). Sets `just_valid` if any signal transitions to VALID this bar,
/// and updates the latch's `confirmed` flag when the validating/invalidating
/// signal *is* the latched one.
#[allow(clippy::too_many_arguments)]
fn update_tracked(
    tracked: &mut [Tracked],
    bar: usize,
    candles: &[Candle],
    cfg: &DetectorConfig,
    printed: Option<&Detected>,
    latch: &mut Option<Latch>,
    latch_signal_bar: Option<usize>,
    just_valid: &mut bool,
) {
    let c = candles[bar];
    let opposer_golden_for = |dir: Direction| -> Option<bool> {
        // The printed signal counts as an opposer only if it's the opposite
        // direction. Its golden-ness gates the protect clause.
        match printed {
            Some(d) if d.direction != dir => {
                let atr = wilder_atr(&candles[..=bar], atr_length_for(cfg.granularity));
                Some(atr.is_some_and(|a| d.is_golden(a)))
            }
            _ => None,
        }
    };

    for t in tracked.iter_mut() {
        if t.state == SigState::Invalid {
            continue;
        }
        let bars_elapsed = bar.saturating_sub(t.signal_bar);
        let mut new_state = t.state;

        // Pending → Valid on a confirming push through the extreme within the
        // window; → Invalid once the window elapses without one. (Closed-bar
        // only — fixes bug #10B.)
        if t.state == SigState::Pending {
            let pushed = match t.direction {
                Direction::Long => c.h > t.high,
                Direction::Short => c.l < t.low,
            };
            if bars_elapsed <= cfg.confirm_bars && pushed {
                new_state = SigState::Valid;
            } else if bars_elapsed > cfg.confirm_bars {
                new_state = SigState::Invalid;
            }
        }

        // Breach of the opposite extreme always invalidates (Pine `low < sig_low`
        // bullish / `high > sig_high` bearish).
        let breached = match t.direction {
            Direction::Long => c.l < t.low,
            Direction::Short => c.h > t.high,
        };
        if breached {
            new_state = SigState::Invalid;
        }

        // Opposing-signal invalidation inside the confirm window, with the
        // golden-protect clause.
        if bars_elapsed <= cfg.confirm_bars
            && new_state != SigState::Invalid
            && let Some(opp_golden) = opposer_golden_for(t.direction)
        {
            let protected = t.golden && !opp_golden;
            if !protected {
                new_state = SigState::Invalid;
            }
        }

        // Apply transition + latch confirmation bookkeeping.
        if new_state != t.state {
            let is_latched = latch_signal_bar == Some(t.signal_bar);
            if new_state == SigState::Valid {
                *just_valid = true;
                if is_latched && let Some(l) = latch.as_mut() {
                    l.confirmed = true;
                }
            } else if t.state != SigState::Invalid
                && new_state == SigState::Invalid
                && is_latched
                && let Some(l) = latch.as_mut()
            {
                l.confirmed = false;
            }
            t.state = new_state;
        }
    }
}

/// Highest high / lowest low over `lookback` bars **strictly preceding**
/// `signal_bar` (Pine `ta.highest(high[1], n)` / `ta.lowest(low[1], n)`).
/// `None` if there are no prior bars.
fn recent_extremes(
    candles: &[Candle],
    signal_bar: usize,
    lookback: usize,
) -> (Option<f64>, Option<f64>) {
    if signal_bar == 0 || lookback == 0 {
        return (None, None);
    }
    let start = signal_bar.saturating_sub(lookback);
    let window = &candles[start..signal_bar];
    if window.is_empty() {
        return (None, None);
    }
    let hi = window.iter().map(|c| c.h).fold(f64::MIN, f64::max);
    let lo = window.iter().map(|c| c.l).fold(f64::MAX, f64::min);
    (Some(hi), Some(lo))
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

    fn cfg() -> DetectorConfig {
        // H1 → ATR length 24; keep windows short by using a tiny confirm window.
        let mut c = DetectorConfig::pine_defaults(Granularity::H1);
        c.confirm_bars = 2;
        c.sl_lookback = 3;
        c
    }

    /// A bullish pinbar on bar 1, then a bar that pushes above its high within
    /// the window → the latched signal confirms.
    fn bullish_pinbar_window() -> Vec<Candle> {
        vec![
            // bar 0 context (prior low 1.05 for the pinbar breakout).
            k("2026-06-16T09:00:00Z", 1.10, 1.12, 1.05, 1.06),
            // bar 1: bullish pinbar. range 1.00..1.20. low 1.00 < 1.05.
            k("2026-06-16T10:00:00Z", 1.16, 1.20, 1.00, 1.18),
            // bar 2: high pushes above the pinbar high (1.20) → confirms the
            // pinbar, but its own shape prints no new signal (balanced body,
            // small wicks both sides, close mid-range — no pinbar/engulfer).
            k("2026-06-16T11:00:00Z", 1.18, 1.22, 1.17, 1.205),
        ]
    }

    #[test]
    fn latches_geometry_on_signal_bar() {
        let candles = bullish_pinbar_window();
        let l = latched_signal_at(&candles, 1, &cfg()).expect("latched");
        assert_eq!(l.direction, Direction::Long);
        assert_eq!(l.kind, SignalKind::Pinbar);
        assert!((l.signal_high - 1.20).abs() < 1e-12);
        assert!((l.signal_low - 1.00).abs() < 1e-12);
        // fresh signal → fires, but not yet confirmed.
        assert!(l.fires);
        assert!(!l.signal_confirmed);
    }

    #[test]
    fn confirms_on_closed_push_within_window() {
        let candles = bullish_pinbar_window();
        // As of bar 2 the push above 1.20 validates the latched pinbar.
        let l = latched_signal_at(&candles, 2, &cfg()).expect("latched");
        assert!(l.signal_confirmed, "validated by the pushing bar");
        // The alert fires on the just-valid bar too.
        assert!(l.fires);
    }

    #[test]
    fn no_confirm_when_push_too_late() {
        let mut candles = bullish_pinbar_window();
        // Replace bar 2 with a non-pushing bar, and add bars past the window.
        candles[2] = k("2026-06-16T11:00:00Z", 1.18, 1.19, 1.17, 1.18); // no push
        candles.push(k("2026-06-16T12:00:00Z", 1.18, 1.19, 1.17, 1.185)); // bar 3
        candles.push(k("2026-06-16T13:00:00Z", 1.30, 1.35, 1.25, 1.34)); // bar 4: push, but window (2) elapsed
        let l = latched_signal_at(&candles, 4, &cfg()).expect("latched");
        assert!(!l.signal_confirmed, "push after confirm_bars doesn't count");
    }

    #[test]
    fn breach_of_low_invalidates_and_unconfirms() {
        let mut candles = bullish_pinbar_window();
        // bar 2 confirms; bar 3 trades below the signal low → invalid, unconfirm.
        candles.push(k("2026-06-16T12:00:00Z", 1.10, 1.12, 0.95, 0.98)); // low < 1.00
        let l = latched_signal_at(&candles, 3, &cfg()).expect("latched");
        // confirmed flips back to false because the latched signal invalidated.
        assert!(!l.signal_confirmed);
    }

    #[test]
    fn recent_extremes_scan_prior_window() {
        let candles = bullish_pinbar_window();
        let l = latched_signal_at(&candles, 1, &cfg()).unwrap();
        // signal_bar = 1, lookback 3 → only bar 0 precedes it.
        assert_eq!(l.recent_high, Some(1.12));
        assert_eq!(l.recent_low, Some(1.05));
    }

    #[test]
    fn no_signal_returns_none() {
        // Flat candles → never a signal.
        let candles = vec![
            k("2026-06-16T09:00:00Z", 1.0, 1.0, 1.0, 1.0),
            k("2026-06-16T10:00:00Z", 1.0, 1.0, 1.0, 1.0),
        ];
        assert!(latched_signal_at(&candles, 1, &cfg()).is_none());
    }
}
