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
//! # Confirmation timing — fixes Pine bug #10B and the early-confirm bug
//!
//! Pine can latch `signal_confirmed = 1` off a bar that hasn't closed (the
//! ADIDAS 5:30-vs-5:45 case). The engine only sees **closed** candles, so a
//! confirming push here is always a closed bar — the port confirms only on
//! closed pushing bars within `confirm_bars`.
//!
//! As of the v2.6 fix, confirmation also resolves **only at the end of the
//! window**: a push through the extreme anywhere inside `confirm_bars` is
//! latched (`Tracked::broke`) and the signal transitions PENDING→VALID when
//! `bars_elapsed == confirm_bars` iff such a push occurred — it no longer
//! validates the instant a bar breaks through. This matches the operator's
//! "wait the full window, then confirm if it ever broke" semantics and the
//! Pine `candle-signals-v2.pine` change; keep the two in sync.
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
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LatchedSignal {
    pub direction: Direction,
    pub kind: SignalKind,
    pub signal_high: f64,
    pub signal_low: f64,
    pub signal_range: f64,
    pub signal_start_time: DateTime<Utc>,
    /// Open-time of the **print bar** (bar `N`) — the bar the signal was
    /// detected on. Distinct from [`signal_start_time`](Self::signal_start_time),
    /// which is the pattern's earliest *covered* bar (`N-1` for 2-bar kinds). The
    /// engine's confirmed-first re-entry watermark
    /// ([`PlanState::last_confirmed_enter_at`](crate::plan_state::PlanState::last_confirmed_enter_at))
    /// stores this exact value so the next scan's `after` bound excludes the
    /// consumed signal precisely (the scan filters on the print bar's time, so a
    /// watermark of `start_time` would be one bar early for 2-bar patterns).
    pub signal_bar_time: DateTime<Utc>,
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
    /// The signal candle's own range (`high - low` for floating; kind-specific
    /// span otherwise) and pattern kind / start time — carried per-signal so a
    /// `LatchedSignal` can be rebuilt for *this* signal (not just the latch) by
    /// [`first_confirmed_signal_at`].
    range: f64,
    kind: SignalKind,
    start_time: DateTime<Utc>,
    /// ATR at the signal's print bar (the value the latch carried at print).
    atr: Option<f64>,
    /// Bar index the signal printed on.
    signal_bar: usize,
    state: SigState,
    golden: bool,
    /// True once price pushed through the extreme at any point *within* the
    /// confirm window. Latched on the breaking bar but only consulted at the
    /// window end (`bars_elapsed == confirm_bars`) — confirmation must wait for
    /// the full window to close, then validate iff a break happened.
    broke: bool,
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
                range: d.geometry.range,
                kind: d.geometry.kind,
                start_time: d.geometry.start_time,
                atr,
                signal_bar: bar,
                state: SigState::Pending,
                golden,
                broke: false,
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
        signal_bar_time: candles[signal_bar].time,
        golden: latch.golden,
        signal_confirmed: latch.confirmed,
        atr: latch.atr,
        recent_high,
        recent_low,
        fires: fired_on_as_of,
    })
}

/// The selection predicate for [`first_confirmed_signal_at`] — the set of
/// filters a confirmed signal must satisfy to *claim the winner slot*.
///
/// # Why this is one struct, not N positional args
///
/// The scan latches the **first** confirmed signal that passes the filters and
/// never revisits the choice (`first_confirmed_bar` is set once). Historically
/// the filters were separate positional arguments the caller assembled by hand,
/// and the recurring bug was a caller **forgetting one**: each of `want_dir`
/// (DE30 2026-07-07), `not_before` (warmup-era signal, bug ①), `after` (QM
/// multi-shot re-consumed its own signal), and `require_golden` (UK 100 v109,
/// a non-golden signal shadowed a later golden one) was added *after* a
/// production miss where the winner slot admitted a signal the enter would never
/// take. Folding them into one value the enter builds **once** from its own
/// intent ([`Self::from_enter`]) makes "did I pass every filter" into "did I
/// build the criteria from the rule" — a single typed call the type system
/// enforces, so a *future* filter is added in exactly two places
/// ([`Self::from_enter`] + [`Self::admits`]) instead of hunted across call sites.
///
/// Each field mirrors an enter-intent gate; see [`Self::admits`] for the exact
/// per-field rule and [`first_confirmed_signal_at`] for the tracking that feeds
/// it. The state machine still **replays every** printed signal (confirmation /
/// invalidation history must be complete regardless of these filters); the
/// criteria only decide which validation may *claim the winner slot*.
#[derive(Debug, Clone, Copy)]
pub struct SignalCriteria {
    /// The plan's trade direction. A confirmed signal in the opposite direction
    /// must not claim the slot (the detector prints both ways; an opposite
    /// signal often confirms first — DE30 2026-07-07's long pinbar).
    pub dir: Direction,
    /// The pattern kind the enter pins, or `None` for "any kind". Prod enters
    /// pass `None` (the enter takes any confirmed pattern in its direction); kept
    /// for parity with the Pine detector's per-pattern alerts and the guard.
    pub kind: Option<SignalKind>,
    /// When the enter demands golden (`intent.needs_golden`), only a golden
    /// confirmed signal may win. A non-golden signal is still *tracked* (it can
    /// golden-protect / invalidate others), it just can't claim the slot —
    /// otherwise it shadows a later golden one the enter would take (UK 100 v109).
    pub require_golden: bool,
    /// The **inclusive** setup floor: only a signal whose print time is
    /// `>= not_before` may win. Scopes "first confirmed" to the setup forming
    /// now, not an ancient warmup-era signal. `None` = no lower bound.
    pub not_before: Option<DateTime<Utc>>,
    /// The **exclusive** re-entry watermark: a signal whose print time is
    /// `<= after` cannot win. How a multi-shot QM enter skips the confirmed
    /// signal it already consumed and advances to the next. `None` = no
    /// watermark (first confirmed in scope wins — the single-shot / first-entry
    /// case). Composes with `not_before`: a winner needs `>= not_before` **and**
    /// `> after`.
    pub after: Option<DateTime<Utc>>,
}

impl SignalCriteria {
    /// Whether a tracked signal may **claim the winner slot**, given its print
    /// time (`candles[t.signal_bar].time`). Pure — the single predicate every
    /// filter funnels through, so adding a filter means adding one clause here.
    /// `require_confirmed` is **not** a field: the scan only ever offers already-
    /// confirmed (freshly-`Valid`) signals to this check, so confirmation is the
    /// caller's gate, not a claim filter (see [`first_confirmed_signal_at`]).
    fn admits(&self, t: &Tracked, print_time: DateTime<Utc>) -> bool {
        // direction (and kind, when pinned)
        let matches = t.direction == self.dir && self.kind.is_none_or(|k| t.kind == k);
        // at/after the inclusive setup floor
        let in_scope = self.not_before.is_none_or(|floor| print_time >= floor);
        // strictly after the exclusive re-entry watermark
        let after_watermark = self.after.is_none_or(|w| print_time > w);
        // golden, when the enter demands it (non-golden is tracked, not claimable)
        let golden_ok = !self.require_golden || t.golden;
        matches && in_scope && after_watermark && golden_ok
    }
}

/// Like [`latched_signal_at`], but returns the **first signal to confirm**
/// rather than the single most-recent latch — the per-signal ("first one wins")
/// entry the operator's rule describes.
///
/// # Why this exists
///
/// [`latched_signal_at`] keeps exactly one latch slot: a fresh print overwrites
/// it, so when an *earlier* signal validates a few bars later, its confirmation
/// is written to the latch **only if that signal is still the latched one**
/// (Pine `signal_high_v == sig_high`; here `is_latched`). In a fast run of
/// back-to-back same-direction signals — e.g. DE30_EUR 2026-07-07, a golden
/// short at 6pm, another short at 7pm — the 7pm print displaces the 6pm signal
/// before its 2-bar confirm window closes, so the 6pm confirmation never
/// surfaces to the engine even though the chart draws its VALID triangle. The
/// engine then never enters a setup the operator can see confirming.
///
/// This function tracks **every** signal to its own resolution and returns the
/// **earliest-printing** signal *matching `want_dir` (and `want_kind` when set)*
/// that has reached `Valid` by `as_of`, carrying *that signal's own* geometry
/// (its base becomes the entry level). "First one wins": the engine's fire-once
/// latch (`state.fired`) blocks the later signals' confirmations once this one
/// has fired the enter.
///
/// **The selection filters are essential**, not cosmetic — see [`SignalCriteria`]
/// for each (direction, kind, golden, setup floor, re-entry watermark) and the
/// production miss that motivated it. In short: the detector prints signals in
/// both directions and across a long warmup tail, so an unfiltered "first
/// confirmed" routinely picks a signal the enter would never take. The criteria
/// mirror the caller's own enter gates so "first confirmed" means "first
/// confirmed signal the enter would actually take". The state machine still
/// **replays every** printed signal (confirmation/invalidation history must be
/// complete); the criteria only decide which validation may *claim the winner
/// slot* via [`SignalCriteria::admits`].
///
/// `fires` is set on the bar the returned signal **just** validated (so the
/// engine's `sig.fires` gate triggers the entry exactly at confirmation). A
/// signal that confirmed on an *earlier* bar than `as_of` still returns (so a
/// re-evaluation after the confirm bar sees the confirmed signal), but with
/// `fires = false` on those later bars — the enter fires once, on the
/// confirmation bar.
pub fn first_confirmed_signal_at(
    candles: &[Candle],
    as_of: usize,
    cfg: &DetectorConfig,
    crit: &SignalCriteria,
) -> Option<LatchedSignal> {
    if as_of >= candles.len() {
        return None;
    }
    let atr_len = atr_length_for(cfg.granularity);
    let mut tracked: Vec<Tracked> = Vec::new();
    // The signal_bar of the first signal that reached Valid, and the bar it did
    // so on (for `fires`). Latched once and never overwritten — first wins.
    let mut first_confirmed_bar: Option<usize> = None;
    let mut confirmed_on: Option<usize> = None;

    for bar in 0..=as_of {
        // ---- 1. Update existing tracked signals against this bar. ----
        let printed = detect_at(candles, bar, &cfg.detect);
        // Snapshot which signals were already Valid so we can detect a
        // *fresh* validation this bar (a PENDING→VALID transition).
        let was_valid: Vec<bool> = tracked.iter().map(|t| t.state == SigState::Valid).collect();
        // `update_tracked` also maintains the single latch, but this path
        // ignores it — pass a throwaway.
        let mut discard_latch: Option<Latch> = None;
        let mut discard_just_valid = false;
        update_tracked(
            &mut tracked,
            bar,
            candles,
            cfg,
            printed.as_ref(),
            &mut discard_latch,
            None,
            &mut discard_just_valid,
        );

        // Record the earliest-printing signal that just reached Valid this bar.
        // Iterate in print order (tracked is push-ordered) and take the first
        // fresh validation; only latch it if we haven't already committed to a
        // first winner.
        if first_confirmed_bar.is_none() {
            for (i, t) in tracked.iter().enumerate() {
                // Only a signal that *just* transitioned PENDING→VALID this bar is
                // a candidate — confirmation is the scan's own gate, so `admits`
                // never sees an unconfirmed signal. The claim filters (direction,
                // kind, setup floor, re-entry watermark, golden) all live in
                // `SignalCriteria::admits` — one predicate, so a future filter is
                // added there, not re-derived at every call site.
                let freshly_valid =
                    t.state == SigState::Valid && !was_valid.get(i).copied().unwrap_or(false);
                if freshly_valid && crit.admits(t, candles[t.signal_bar].time) {
                    first_confirmed_bar = Some(t.signal_bar);
                    confirmed_on = Some(bar);
                    break;
                }
            }
        }

        // ---- 2. Capture a new signal printing this bar. ----
        if let Some(d) = printed {
            let atr = wilder_atr(&candles[..=bar], atr_len);
            let golden = atr.is_some_and(|a| d.is_golden(a));
            tracked.push(Tracked {
                direction: d.direction,
                high: d.geometry.high,
                low: d.geometry.low,
                range: d.geometry.range,
                kind: d.geometry.kind,
                start_time: d.geometry.start_time,
                atr,
                signal_bar: bar,
                state: SigState::Pending,
                golden,
                broke: false,
            });
        }
    }

    // The winning signal's own tracked entry (its confirmation may have been
    // followed by an invalidation on a later bar — but "first confirmed wins"
    // fires the enter on the confirmation bar, so we report it confirmed as of
    // that bar). Look it up by signal_bar.
    let winner_bar = first_confirmed_bar?;
    let t = tracked.iter().find(|t| t.signal_bar == winner_bar)?;
    let (recent_high, recent_low) = recent_extremes(candles, t.signal_bar, cfg.sl_lookback);
    Some(LatchedSignal {
        direction: t.direction,
        kind: t.kind,
        signal_high: t.high,
        signal_low: t.low,
        signal_range: t.range,
        signal_start_time: t.start_time,
        signal_bar_time: candles[t.signal_bar].time,
        golden: t.golden,
        signal_confirmed: true,
        atr: t.atr,
        recent_high,
        recent_low,
        // Fire only on the confirmation bar (the bar the winner validated).
        fires: confirmed_on == Some(as_of),
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

        // Pending signals latch a confirming push (closed-bar only — fixes bug
        // #10B) but only RESOLVE at the end of the window: confirmation must
        // wait the full `confirm_bars`, then validate iff a push happened at any
        // point inside the window. Resolving early (the moment a bar broke the
        // extreme) was the v2.5 bug — it fired the confirmation before the
        // operator's "wait 2 bar closes" window had elapsed.
        if t.state == SigState::Pending {
            let pushed = match t.direction {
                Direction::Long => c.h > t.high,
                Direction::Short => c.l < t.low,
            };
            if bars_elapsed <= cfg.confirm_bars && pushed {
                t.broke = true;
            }
            if bars_elapsed >= cfg.confirm_bars {
                new_state = if t.broke {
                    SigState::Valid
                } else {
                    SigState::Invalid
                };
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

    /// Test shim for [`first_confirmed_signal_at`] preserving the pre-refactor
    /// positional filter order (dir / kind / golden / not_before / after) so the
    /// scan tests read the same — it just folds them into a `SignalCriteria`.
    fn fc(
        candles: &[Candle],
        as_of: usize,
        cfg: &DetectorConfig,
        dir: Direction,
        kind: Option<SignalKind>,
        require_golden: bool,
        not_before: Option<DateTime<Utc>>,
        after: Option<DateTime<Utc>>,
    ) -> Option<LatchedSignal> {
        first_confirmed_signal_at(
            candles,
            as_of,
            cfg,
            &SignalCriteria {
                dir,
                kind,
                require_golden,
                not_before,
                after,
            },
        )
    }

    /// A minimal `Tracked` for exercising `SignalCriteria::admits` in isolation
    /// (only the fields `admits` reads matter — direction, kind, golden; the rest
    /// are inert placeholders). `signal_bar`/geometry are unused by `admits`.
    fn tracked(dir: Direction, kind: SignalKind, golden: bool) -> Tracked {
        Tracked {
            direction: dir,
            high: 1.0,
            low: 0.0,
            range: 1.0,
            kind,
            start_time: ts("2026-01-01T00:00:00Z"),
            atr: Some(0.5),
            signal_bar: 0,
            state: SigState::Valid,
            golden,
            broke: true,
        }
    }

    /// The wide-open criteria (no filters engaged) — the baseline every
    /// per-filter test tightens one field of.
    fn any_long() -> SignalCriteria {
        SignalCriteria {
            dir: Direction::Long,
            kind: None,
            require_golden: false,
            not_before: None,
            after: None,
        }
    }

    #[test]
    fn admits_open_criteria_takes_any_matching_direction() {
        let t = tracked(Direction::Long, SignalKind::Pinbar, false);
        assert!(any_long().admits(&t, ts("2026-06-16T10:00:00Z")));
    }

    #[test]
    fn admits_filters_by_direction() {
        // opposite-direction signal never claims the slot (DE30 long-pinbar case)
        let short = tracked(Direction::Short, SignalKind::Pinbar, true);
        assert!(!any_long().admits(&short, ts("2026-06-16T10:00:00Z")));
    }

    #[test]
    fn admits_filters_by_kind_when_pinned() {
        let crit = SignalCriteria {
            kind: Some(SignalKind::Pinbar),
            ..any_long()
        };
        let pinbar = tracked(Direction::Long, SignalKind::Pinbar, false);
        let engulfer = tracked(Direction::Long, SignalKind::FloatingEngulfer, false);
        assert!(crit.admits(&pinbar, ts("2026-06-16T10:00:00Z")));
        assert!(
            !crit.admits(&engulfer, ts("2026-06-16T10:00:00Z")),
            "a pinned kind excludes other kinds"
        );
    }

    #[test]
    fn admits_requires_golden_only_when_asked() {
        let non_golden = tracked(Direction::Long, SignalKind::Pinbar, false);
        // default: golden not required → a non-golden signal is admitted
        assert!(any_long().admits(&non_golden, ts("2026-06-16T10:00:00Z")));
        // require_golden: a non-golden signal can't claim the slot (UK 100 v109)
        let gated = SignalCriteria {
            require_golden: true,
            ..any_long()
        };
        assert!(!gated.admits(&non_golden, ts("2026-06-16T10:00:00Z")));
        let golden = tracked(Direction::Long, SignalKind::Pinbar, true);
        assert!(gated.admits(&golden, ts("2026-06-16T10:00:00Z")));
    }

    #[test]
    fn admits_setup_floor_is_inclusive() {
        let floor = ts("2026-06-16T10:00:00Z");
        let crit = SignalCriteria {
            not_before: Some(floor),
            ..any_long()
        };
        let t = tracked(Direction::Long, SignalKind::Pinbar, false);
        assert!(
            !crit.admits(&t, floor - chrono::Duration::hours(1)),
            "before floor excluded"
        );
        assert!(
            crit.admits(&t, floor),
            "exactly at floor is admitted (inclusive)"
        );
        assert!(
            crit.admits(&t, floor + chrono::Duration::hours(1)),
            "after floor admitted"
        );
    }

    #[test]
    fn admits_reentry_watermark_is_exclusive() {
        let mark = ts("2026-06-16T10:00:00Z");
        let crit = SignalCriteria {
            after: Some(mark),
            ..any_long()
        };
        let t = tracked(Direction::Long, SignalKind::Pinbar, false);
        assert!(
            !crit.admits(&t, mark),
            "exactly at the watermark is excluded (exclusive)"
        );
        assert!(
            !crit.admits(&t, mark - chrono::Duration::hours(1)),
            "before the watermark excluded"
        );
        assert!(
            crit.admits(&t, mark + chrono::Duration::hours(1)),
            "strictly after admitted"
        );
    }

    #[test]
    fn admits_composes_floor_and_watermark() {
        // a winner must satisfy >= not_before AND > after
        let floor = ts("2026-06-16T09:00:00Z");
        let mark = ts("2026-06-16T11:00:00Z");
        let crit = SignalCriteria {
            not_before: Some(floor),
            after: Some(mark),
            ..any_long()
        };
        let t = tracked(Direction::Long, SignalKind::Pinbar, false);
        // in [floor, mark] but not > mark → excluded by the watermark
        assert!(!crit.admits(&t, ts("2026-06-16T10:00:00Z")));
        // > mark (and >= floor) → admitted
        assert!(crit.admits(&t, ts("2026-06-16T12:00:00Z")));
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
        // The push above 1.20 lands on bar 2, which is the window-end bar
        // (signal_bar = 1, confirm_bars = 2 → window ends at bar_index 3? no:
        // bar 2 → bars_elapsed = 1). Confirmation must therefore wait one more
        // bar; see early-vs-late tests below for the precise timing.
        // As of bar 2 (mid-window) it has NOT yet confirmed.
        let l2 = latched_signal_at(&candles, 2, &cfg()).expect("latched");
        assert!(
            !l2.signal_confirmed,
            "mid-window push doesn't confirm early"
        );
    }

    /// Window-end-only confirmation: a push that happens *early* in the window
    /// (bar 2, bars_elapsed = 1) must NOT confirm until the window actually
    /// elapses (bar 3, bars_elapsed = 2). This is the v2.5 bug fix — the old
    /// code fired the confirmation the instant a bar broke the extreme.
    fn early_break_then_hold() -> Vec<Candle> {
        let mut c = bullish_pinbar_window();
        // bar 2 already pushes above 1.20 (early break, bars_elapsed = 1).
        // bar 3: a balanced doji that stays inside the range (no further push
        // above 1.20, no breach of the low 1.00, and prints no new signal) —
        // the latched break should still carry confirmation to window end.
        c.push(k("2026-06-16T12:00:00Z", 1.18, 1.185, 1.175, 1.18));
        c
    }

    #[test]
    fn no_confirm_before_window_end() {
        let candles = early_break_then_hold();
        // As of bar 2 the break has happened but the window (2 bars) is not yet
        // closed — confirmation must wait, and the alert must not fire.
        let l = latched_signal_at(&candles, 2, &cfg()).expect("latched");
        assert!(!l.signal_confirmed, "must wait the full window");
        assert!(!l.fires, "no early just-valid alert fire");
    }

    #[test]
    fn confirms_at_window_end_after_early_break() {
        let candles = early_break_then_hold();
        // As of bar 3 (bars_elapsed = 2 = confirm_bars) the window has closed
        // and the in-window break carries confirmation.
        let l = latched_signal_at(&candles, 3, &cfg()).expect("latched");
        assert!(l.signal_confirmed, "in-window break confirms at window end");
        assert!(l.fires, "alert fires on the window-end just-valid bar");
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

    /// Two back-to-back short floating engulfers (a descending staircase), the
    /// DE30_EUR 2026-07-07 shape in miniature. The FIRST (bar 1) signal confirms
    /// — the next bars push below its low without breaching its high — but a
    /// SECOND short prints at bar 2 and overwrites the single latch before bar 1
    /// resolves. `latched_signal_at` therefore reports the *second* signal (and
    /// does not surface bar 1's confirmation); `first_confirmed_signal_at`
    /// reports the *first* signal with its own base as the level.
    fn two_short_engulfers_first_confirms() -> Vec<Candle> {
        vec![
            // bar 0: bearish context so bar 1 can engulf it.
            k("2026-06-16T09:00:00Z", 120.0, 121.0, 118.0, 118.5),
            // bar 1: short floating engulfer #1 ("6pm"). bearish, close 110.5 <
            // prev.low 118, close in bottom-25% of 110..117.5. high 117.5 / low 110.
            k("2026-06-16T10:00:00Z", 117.0, 117.5, 110.0, 110.5),
            // bar 2: short floating engulfer #2 ("7pm"). bearish, close 104.5 <
            // prev.low 110 → this bar's low (104) pushes below bar1's low (110),
            // a confirming push for bar 1; its own high 112 stays *below* bar1's
            // high 117.5 so it does NOT breach/invalidate bar 1. high 112 / low 104.
            k("2026-06-16T11:00:00Z", 110.0, 112.0, 104.0, 104.5),
            // bar 3: window-end for bar 1 (bars_elapsed = 2). Prints NO signal —
            // no breakout (low 104.5 > bar2.low 104, so no bullish pinbar), not
            // an engulfer, highs stay under both signals' highs so nothing is
            // breached. Bar 1 validates here off the bar-2 push.
            k("2026-06-16T12:00:00Z", 105.0, 106.0, 104.5, 105.3),
        ]
    }

    #[test]
    fn latched_reports_the_second_signal_missing_first_confirmation() {
        // Baseline / bug-characterisation: the single latch holds the *second*
        // signal at bar 3, and its confirmation state is the second signal's
        // (not bar 1's) — exactly why the engine misses the entry.
        let candles = two_short_engulfers_first_confirms();
        let l = latched_signal_at(&candles, 3, &cfg()).expect("latched");
        assert_eq!(l.direction, Direction::Short);
        // The latch is the SECOND signal (bar 2): high 112 / low 104.
        assert!((l.signal_high - 112.0).abs() < 1e-9, "hi {}", l.signal_high);
        assert!((l.signal_low - 104.0).abs() < 1e-9, "lo {}", l.signal_low);
    }

    #[test]
    fn first_confirmed_reports_the_first_signal_with_its_own_base() {
        // The fix: the FIRST signal (bar 1) confirmed at bar 3, so
        // `first_confirmed_signal_at` returns bar 1's geometry — its own low
        // (110) is the base the entry anchors to — with fires=true on the
        // confirmation bar.
        let candles = two_short_engulfers_first_confirms();
        let f = fc(
            &candles,
            3,
            &cfg(),
            Direction::Short,
            None,
            false,
            None,
            None,
        )
        .expect("first-confirmed");
        assert_eq!(f.direction, Direction::Short);
        assert!(f.signal_confirmed, "first signal is confirmed");
        assert!(f.fires, "fires on the confirmation bar");
        // Bar 1's OWN geometry — high 117.5 / low 110 — not the second signal's.
        assert!((f.signal_high - 117.5).abs() < 1e-9, "hi {}", f.signal_high);
        assert!((f.signal_low - 110.0).abs() < 1e-9, "lo {}", f.signal_low);
    }

    #[test]
    fn first_confirmed_only_fires_on_the_confirmation_bar() {
        // Before the confirmation bar there is no confirmed signal yet; after
        // it, the signal is still reported (so a later re-eval sees it) but
        // fires=false — the enter fires once, on the confirmation bar.
        let candles = two_short_engulfers_first_confirms();
        // bar 2: bar 1 not yet resolved (bars_elapsed = 1) → nothing confirmed.
        assert!(
            fc(
                &candles,
                2,
                &cfg(),
                Direction::Short,
                None,
                false,
                None,
                None
            )
            .is_none(),
            "no confirmation before the window closes"
        );
        // bar 3: confirmed, fires.
        let at3 = fc(
            &candles,
            3,
            &cfg(),
            Direction::Short,
            None,
            false,
            None,
            None,
        )
        .expect("confirmed at 3");
        assert!(at3.fires, "fires on confirmation bar");
    }

    #[test]
    fn first_confirmed_filters_by_direction() {
        // The two engulfers are SHORT. Asking for a confirmed LONG in the same
        // window returns None — an opposite-direction confirmation must never be
        // handed to a short enter (the DE30 long-pinbar-confirmed-first bug: the
        // filter is what stops the caller latching onto the wrong signal and
        // declining, missing the confirmed short entirely).
        let candles = two_short_engulfers_first_confirms();
        assert!(
            fc(
                &candles,
                3,
                &cfg(),
                Direction::Long,
                None,
                false,
                None,
                None
            )
            .is_none(),
            "no confirmed LONG in a short-only window"
        );
    }

    /// Real UK 100 H1 candles, 2026-07-13T13:00Z .. 2026-07-14T13:00Z (mid),
    /// the `--quasimodo` iH&S window where every golden Long was rejected. A
    /// **non-golden** Long confirms early (07-14 03:00Z prints, validates 05:00Z,
    /// size ≈24 < ATR) and — under "first confirmed wins" — permanently shadows
    /// the **golden** Long (07-14 11:00Z prints, validates 13:00Z, size 36.1).
    /// The regression: a `want_golden` scan must skip the non-golden signal and
    /// return the golden one confirming at 13:00Z.
    fn uk100_quasimodo_golden_shadow() -> Vec<Candle> {
        vec![
            // ATR warmup tail (H1 ATR length 24) so the golden test is honest.
            k("2026-07-12T23:00:00Z", 10509.5, 10509.9, 10502.1, 10508.9),
            k("2026-07-13T00:00:00Z", 10508.2, 10520.0, 10494.7, 10496.8),
            k("2026-07-13T01:00:00Z", 10497.2, 10498.4, 10479.8, 10485.7),
            k("2026-07-13T02:00:00Z", 10485.7, 10493.0, 10478.7, 10478.8),
            k("2026-07-13T03:00:00Z", 10478.9, 10479.7, 10467.5, 10472.2),
            k("2026-07-13T04:00:00Z", 10472.3, 10477.5, 10463.0, 10464.0),
            k("2026-07-13T05:00:00Z", 10463.5, 10478.8, 10450.8, 10474.0),
            k("2026-07-13T06:00:00Z", 10473.8, 10501.8, 10466.5, 10495.5),
            k("2026-07-13T07:00:00Z", 10496.0, 10533.5, 10477.3, 10486.5),
            k("2026-07-13T08:00:00Z", 10486.8, 10515.8, 10474.5, 10515.8),
            k("2026-07-13T09:00:00Z", 10516.0, 10518.8, 10489.5, 10496.0),
            k("2026-07-13T10:00:00Z", 10496.3, 10497.3, 10469.2, 10480.4),
            k("2026-07-13T11:00:00Z", 10480.7, 10500.4, 10473.3, 10475.7),
            k("2026-07-13T12:00:00Z", 10476.0, 10491.9, 10463.9, 10490.9),
            k("2026-07-13T13:00:00Z", 10491.2, 10521.5, 10474.8, 10516.1),
            k("2026-07-13T14:00:00Z", 10516.3, 10517.5, 10468.0, 10504.7),
            k("2026-07-13T15:00:00Z", 10505.0, 10506.0, 10485.6, 10494.2),
            k("2026-07-13T16:00:00Z", 10494.0, 10502.2, 10470.4, 10488.8),
            k("2026-07-13T17:00:00Z", 10489.3, 10506.9, 10488.8, 10497.5),
            k("2026-07-13T18:00:00Z", 10497.8, 10502.2, 10487.9, 10497.1),
            k("2026-07-13T19:00:00Z", 10497.6, 10502.4, 10488.9, 10501.4),
            k("2026-07-13T20:00:00Z", 10498.5, 10501.4, 10490.8, 10492.7),
            k("2026-07-13T22:00:00Z", 10496.9, 10500.4, 10489.7, 10490.7),
            k("2026-07-13T23:00:00Z", 10490.6, 10492.4, 10480.8, 10491.7),
            k("2026-07-14T00:00:00Z", 10498.5, 10500.7, 10427.8, 10444.4),
            k("2026-07-14T01:00:00Z", 10444.3, 10469.5, 10431.2, 10438.1),
            k("2026-07-14T02:00:00Z", 10438.2, 10462.1, 10437.5, 10450.1),
            k("2026-07-14T03:00:00Z", 10450.3, 10459.7, 10435.4, 10459.2),
            k("2026-07-14T04:00:00Z", 10459.4, 10481.7, 10451.9, 10478.5),
            k("2026-07-14T05:00:00Z", 10478.7, 10501.2, 10476.8, 10492.3),
            k("2026-07-14T06:00:00Z", 10492.5, 10507.9, 10465.3, 10467.3),
            k("2026-07-14T07:00:00Z", 10468.3, 10496.2, 10425.7, 10447.4),
            k("2026-07-14T08:00:00Z", 10447.6, 10462.2, 10424.2, 10425.4),
            k("2026-07-14T09:00:00Z", 10425.7, 10460.1, 10422.2, 10454.9),
            k("2026-07-14T10:00:00Z", 10455.1, 10464.2, 10448.7, 10457.7),
            k("2026-07-14T11:00:00Z", 10458.0, 10484.1, 10448.0, 10475.9),
            k("2026-07-14T12:00:00Z", 10476.1, 10515.0, 10451.5, 10495.8),
            k("2026-07-14T13:00:00Z", 10496.1, 10544.3, 10488.7, 10535.6),
        ]
    }

    #[test]
    fn non_golden_confirmation_does_not_shadow_a_later_golden() {
        let candles = uk100_quasimodo_golden_shadow();
        let c = DetectorConfig::pine_defaults(Granularity::H1);
        let as_of = candles.len() - 1; // 07-14 13:00Z, the golden confirmation bar

        // Baseline (want_golden = false): the non-golden early Long wins and
        // permanently shadows the golden one. It printed at 07-14 03:00Z and is
        // NOT golden — exactly the signal the golden-requiring enter can't take.
        let no_gate = fc(
            &candles,
            as_of,
            &c,
            Direction::Long,
            None,
            false,
            None,
            None,
        )
        .expect("some confirmed long without the golden gate");
        assert!(
            !no_gate.golden,
            "baseline: the early non-golden Long claims the slot (golden={})",
            no_gate.golden
        );
        assert_eq!(
            no_gate.signal_bar_time,
            ts("2026-07-14T03:00:00Z"),
            "baseline winner is the 03:00Z non-golden signal"
        );

        // The fix (want_golden = true): the non-golden 03:00Z signal is skipped;
        // the golden 11:00Z Long claims the slot and fires on its 13:00Z
        // confirmation bar with its own geometry (high 10484.1 / low 10448.0).
        let gated = fc(&candles, as_of, &c, Direction::Long, None, true, None, None)
            .expect("the golden long is found once non-golden signals can't claim the slot");
        assert!(gated.golden, "the winner is golden");
        assert!(gated.signal_confirmed, "and confirmed");
        assert!(gated.fires, "fires on its 13:00Z confirmation bar");
        assert_eq!(
            gated.signal_bar_time,
            ts("2026-07-14T11:00:00Z"),
            "the golden winner printed at 11:00Z"
        );
        assert!(
            (gated.signal_high - 10484.1).abs() < 1e-6 && (gated.signal_low - 10448.0).abs() < 1e-6,
            "carries the 11:00Z signal's own geometry: hi {} lo {}",
            gated.signal_high,
            gated.signal_low
        );
    }

    #[test]
    fn first_confirmed_respects_the_not_before_floor() {
        // With a floor past the first signal's print bar, that signal can no
        // longer claim the winner slot — mirroring the engine scoping "first
        // confirmed" to the setup (past break-and-close / replay-start) so a
        // warmup-era signal isn't picked. Here the only confirmed short prints on
        // bar 1; a floor at bar 2's time excludes it → None.
        let candles = two_short_engulfers_first_confirms();
        let floor = candles[2].time; // strictly after the bar-1 signal print
        assert!(
            fc(
                &candles,
                3,
                &cfg(),
                Direction::Short,
                None,
                false,
                Some(floor),
                None
            )
            .is_none(),
            "a signal printing before the floor can't win"
        );
        // A floor at the signal's own print bar still admits it (>= is inclusive).
        let floor_incl = candles[1].time;
        assert!(
            fc(
                &candles,
                3,
                &cfg(),
                Direction::Short,
                None,
                false,
                Some(floor_incl),
                None,
            )
            .is_some(),
            "the floor is inclusive of a signal printing exactly at it"
        );
    }

    /// A descending staircase of short signals, the multi-shot QM re-entry shape.
    /// Signal A (bar 1, high 117.5 / low 110) confirms on bar 3. Further short
    /// signals print lower down the staircase — the next one to confirm after A
    /// is the bar-4 engulfer (high 112 / low 108), which validates on bar 6. This
    /// is exactly the live case: after entering + stopping out on A, the plan
    /// re-enters on the *next* confirmed short, not on A forever.
    fn descending_short_staircase() -> Vec<Candle> {
        vec![
            // bar 0: bearish context for signal A to engulf.
            k("2026-06-16T09:00:00Z", 120.0, 121.0, 118.0, 118.5),
            // bar 1: short engulfer A. high 117.5 / low 110.
            k("2026-06-16T10:00:00Z", 117.0, 117.5, 110.0, 110.5),
            // bar 2: pushes below A's low (109 < 110) without breaching A's high →
            // confirming push for A.
            k("2026-06-16T11:00:00Z", 110.2, 111.0, 109.0, 109.5),
            // bar 3: A's window-end (elapsed 2) → A validates. Prints no signal.
            k("2026-06-16T12:00:00Z", 110.0, 111.5, 109.6, 110.8),
            // bar 4: the next short down the staircase (engulfs bar 3). high 112 /
            // low 108 — the signal a re-entry should take once A is consumed.
            k("2026-06-16T13:00:00Z", 111.0, 112.0, 108.0, 108.5),
            // bar 5: prints its own lower short and pushes below bar-4's low
            // (100 < 108) without breaching bar-4's high → confirming push for
            // bar-4.
            k("2026-06-16T14:00:00Z", 107.0, 107.5, 100.0, 100.5),
            // bar 6: bar-4's window-end (elapsed 2) → bar-4 validates.
            k("2026-06-16T15:00:00Z", 100.2, 101.0, 99.0, 99.5),
        ]
    }

    #[test]
    fn after_watermark_advances_to_the_next_confirmed_short() {
        // The multi-shot QM re-entry fix. Without `after`, the confirmed-first
        // scan is frozen on signal A forever (fires only on A's one confirmation
        // bar). With `after` = A's print time, A is excluded and the *next*
        // confirmed short down the staircase becomes the winner — on its own,
        // later, confirmation bar.
        let candles = descending_short_staircase();

        // No watermark: A wins on its confirmation bar (bar 3), fires there, and
        // never fires again (frozen — the exact multi-shot bug).
        let a = fc(
            &candles,
            3,
            &cfg(),
            Direction::Short,
            None,
            false,
            None,
            None,
        )
        .expect("A confirmed at bar 3");
        assert!(a.fires, "A fires on its confirmation bar");
        assert!(
            (a.signal_high - 117.5).abs() < 1e-9,
            "A hi {}",
            a.signal_high
        );
        assert_eq!(
            a.signal_bar_time, candles[1].time,
            "A's print-bar watermark"
        );
        let a_watermark = a.signal_bar_time;
        // Frozen: at bar 6, still A, no longer firing — this is what starves the
        // re-entry without the watermark.
        let frozen = fc(
            &candles,
            6,
            &cfg(),
            Direction::Short,
            None,
            false,
            None,
            None,
        )
        .expect("still A at bar 6");
        assert!((frozen.signal_high - 117.5).abs() < 1e-9);
        assert!(
            !frozen.fires,
            "A no longer fires after its confirmation bar"
        );

        // With `after` = A's print time, A can no longer win. Before the next
        // signal confirms (bar 3) → None; on the next signal's confirmation bar
        // (bar 6) → the next short wins and fires.
        assert!(
            fc(
                &candles,
                3,
                &cfg(),
                Direction::Short,
                None,
                false,
                None,
                Some(a_watermark)
            )
            .is_none(),
            "A is excluded by the watermark and no later short has confirmed yet"
        );
        let next = fc(
            &candles,
            6,
            &cfg(),
            Direction::Short,
            None,
            false,
            None,
            Some(a_watermark),
        )
        .expect("next confirmed short at bar 6");
        assert!(
            next.fires,
            "the next short fires on its own confirmation bar"
        );
        // A DIFFERENT signal than A: the bar-4 engulfer (high 112 / low 108),
        // printing strictly after A.
        assert!(
            (next.signal_high - 112.0).abs() < 1e-9,
            "next hi {}",
            next.signal_high
        );
        assert!(
            next.signal_bar_time > a_watermark,
            "next signal prints after A"
        );
    }
}
