//! [`Retest`] — the retest prep rule, **fact-based & pure**.
//!
//! The v2 rewrite of the second prep, and the first fact **consumer** in the
//! engine: it does nothing until break-and-close has stamped
//! `(line, "break_close")`, then — on a genuine retest cross of the same line
//! strictly after the break — writes `(line, "retest")`. Like
//! [`BreakAndClose`](super::BreakAndClose) it only *reads* the
//! [`Facts`](crate::facts::Facts) blackboard and mutates nothing; every output
//! leaves as an [`Effect`] for the driver to apply.
//!
//! # What it reproduces from the proven v1 logic (`engine::evaluate::stamp_retest`)
//!
//! 1. **Latch** — if `(line, "retest")` is already set, do nothing. (v1:
//!    `state.retest_seen_at.is_some()`.)
//! 2. **Producer gate** — read `(line, "break_close")`; if unset, do nothing.
//!    The retest is meaningless until the neckline has been broken and closed.
//!    (v1: `effective_break_at` returning the stamped break time.)
//! 3. **After-the-break window** — only candles strictly after the break time
//!    are considered (v1: `if candle.time <= break_at { return }`).
//! 4. **Time-decaying closeness tolerance** — the first bar after the break must
//!    reach the line; each later bar loosens the near side by
//!    `retest_atr_step × ATR` per bar (see [`retest_tolerance`]).
//! 5. **`last_close` bookkeeping before the gate** — an `OnClose` retest
//!    measures this close against the rule's *prior* close, held as rule-private
//!    scratch keyed `(rule_id, "last_close")`. Emitted on **every** past-gate
//!    tick (even a spread-hour bar, even a non-firing bar, even the ATR-None
//!    path) so a genuine retest on the next clean bar is measured correctly.
//! 6. **Spread-hour suppression gates the STAMP, not the bookkeeping** — a
//!    retest cross landing on a learned spread hour is a liquidity-vacuum wick,
//!    not a genuine retest: don't stamp it (but the `last_close` scratch above
//!    still records).
//!
//! # NO `Fire` (divergence from v1, by design)
//!
//! v1 emitted a `push_fire` prep here because `run_enter`'s prep gate was
//! store-backed — the retest had to *dispatch* a `Prep` intent to seed the
//! store the enter read. In v2 the enter reads the `(line, "retest")` **fact**
//! directly, so the fact replaces that store round-trip: the retest is
//! **fact-only**. If a later enter rule turns out to need a dispatched prep,
//! add the `Fire` then.
//!
//! # ATR-None handling (divergence from v1's panic, by design)
//!
//! v1's `retest_tolerance` `.expect()`s the Wilder ATR — a deliberate hard-fail,
//! justified because by the retest phase the window is always warm past
//! `atr_length_for(granularity)`. A pure v2 rule can't panic-as-contract cleanly
//! and panicking inside the driver's tick loop is worse than a loud log, so this
//! port surfaces the (structurally-unreachable) `None` via `tracing::error!` and
//! **does not stamp** — while still emitting the `last_close` scratch. Same
//! "surface loudly, don't silently paper over" intent, expressed non-fatally.
//! (v1 keeps its `.expect()`; only the v2 driver-loop context needs the softer
//! landing.)

use trade_control_core::broker::Candle;
use trade_control_core::signals::{atr_length_for, wilder_atr};
use trade_control_core::trade_plan::{BarEvent, CrossDir, Trigger};

use crate::cross::{eval_trigger, level_crossed, line_price_at, trigger_uses_close};
use crate::effect::Effect;
use crate::facts::FactValue;
use crate::plan::{Line, PlanRule};
use crate::rule::Rule;
use crate::world::World;

/// Shared-fact kind for the retest stamp (keyed `(line, kind)`).
const KIND_RETEST: &str = "retest";
/// Shared-fact kind of the *producer* fact this rule gates on.
const KIND_BREAK_CLOSE: &str = "break_close";
/// Rule-private scratch kind for the prior-close bookkeeping (keyed
/// `(rule_id, kind)`).
const SCRATCH_LAST_CLOSE: &str = "last_close";

/// The retest prep, bound to a v2 [`PlanRule`]. Borrowed so instantiating it per
/// tick is free.
pub struct Retest<'r> {
    /// The rule this wraps.
    pub rule: &'r PlanRule,
}

impl<'r> Retest<'r> {
    /// Wrap a retest [`PlanRule`].
    pub fn new(rule: &'r PlanRule) -> Self {
        Self { rule }
    }
}

impl Rule for Retest<'_> {
    fn rule_id(&self) -> &str {
        &self.rule.id
    }

    fn tick(&self, w: &World) -> Vec<Effect> {
        // Latch: once the retest fact is set, this rule is done — it never
        // re-stamps (v1's `retest_seen_at.is_some()` guard).
        if w.facts.is_set(&self.rule.line, KIND_RETEST) {
            return Vec::new();
        }

        // Producer gate: nothing to do until break-and-close has stamped.
        let Some(break_at) = w.facts.at(&self.rule.line, KIND_BREAK_CLOSE) else {
            return Vec::new();
        };

        // The current closed bar (last of the window). Absent only for an empty
        // window, which the driver already guards.
        let Some(candle) = w.current() else {
            return Vec::new();
        };

        // Only candles strictly after the break count as a retest.
        if candle.time <= break_at {
            return Vec::new();
        }

        // The line this rule references must exist in the plan.
        let Some(line) = w.plan.line(&self.rule.line) else {
            return Vec::new();
        };

        let trigger = self.trigger_for(line, w.plan.granularity.seconds());

        let mut effects = Vec::new();

        // `last_close` bookkeeping BEFORE the gate — emitted on every past-gate
        // tick (firing or not, spread-hour or not, ATR-None or not), as a
        // rule-private scratch write for the driver to apply. Only `OnClose`
        // triggers track it. Pushed early so even the no-stamp paths record it.
        if trigger_uses_close(&trigger) {
            effects.push(Effect::WriteScratch {
                rule_id: self.rule.id.clone(),
                kind: SCRATCH_LAST_CLOSE.to_string(),
                value: FactValue::Num(candle.c),
            });
        }

        // Time-decaying closeness tolerance. `None` ⇒ ATR could not be computed
        // (structurally unreachable by the retest phase — the window is warm
        // past `atr_length_for(granularity)` once break_close is set). Surface
        // loudly and do NOT stamp; the `last_close` scratch above still records.
        let Some(tol) = self.retest_tolerance(break_at, candle, w.window, w.plan) else {
            // Surface loudly and do NOT stamp. This branch is structurally
            // unreachable once break_close is stamped (the window is warm past
            // `atr_length_for(granularity)` by then), so a hit means a mis-sized
            // window upstream — logged, not swallowed. The `last_close` scratch
            // above still recorded.
            let atr_length = atr_length_for(w.plan.granularity);
            tracing::error!(
                rule_id = %self.rule.id,
                instrument = %self.rule.intent.instrument,
                granularity = ?w.plan.granularity,
                window_len = w.window.len(),
                atr_length,
                "retest tolerance needs Wilder ATR but the window is too short for \
                 atr_length_for(granularity); should be unreachable once break_close is \
                 stamped — mis-sized window upstream; not stamping the retest this bar",
            );
            return effects;
        };

        let prev_close = w.facts.num_scratch(&self.rule.id, SCRATCH_LAST_CLOSE);
        let crossed = retest_crossed(
            &trigger,
            candle,
            prev_close,
            w.window,
            w.plan.cross_buffer_pct,
            tol,
        );

        // Spread-hour "rubbish candle": a retest cross on a learned spread hour
        // is a liquidity-vacuum wick, not a genuine retest — don't stamp. Gates
        // the stamp only; the `last_close` scratch already recorded above.
        let spread_hour = trade_control_core::spread_blackout::is_spread_hour(
            &self.rule.intent.instrument,
            candle.time,
        );

        if crossed && !spread_hour {
            // Genuine retest: stamp the shared fact. No `Fire` — the enter reads
            // this fact directly (see module docs).
            effects.push(Effect::WriteFact {
                line: self.rule.line.clone(),
                kind: KIND_RETEST.to_string(),
                value: FactValue::At(candle.time),
            });
        }

        effects
    }
}

impl Retest<'_> {
    /// Assemble the v1 [`Trigger::TrendlineCross`] that the cross helpers
    /// evaluate, from the v2 line + the rule's bar/dir. A horizontal line falls
    /// out as `a.price == b.price`. Same shape as
    /// [`BreakAndClose`](super::BreakAndClose)'s `trigger_for`.
    fn trigger_for(&self, line: &Line, bar_seconds: i64) -> Trigger {
        Trigger::TrendlineCross {
            a: line.a,
            b: line.b,
            // A neckline always projects forward past its second anchor.
            extend_forward: true,
            bar_seconds,
            dir: self.rule.dir,
            bar: self.rule.bar,
        }
    }

    /// The retest's near-side closeness tolerance for this bar, in **price
    /// units** — a faithful port of v1's `retest_tolerance`, returning `None`
    /// where v1 panics.
    ///
    /// `(N - 1) × plan.retest_atr_step × ATR`, where `N` is the number of bars
    /// in `window` strictly after `break_at` up to & including `candle` (first
    /// bar after the break = 1, so its tolerance is 0 — it must reach the line).
    /// ATR is the Wilder ATR at this bar over `window`.
    ///
    /// `N <= 1` ⇒ `Some(0.0)` (no ATR needed). Otherwise `None` iff
    /// [`wilder_atr`] returns `None` (the mis-sized-window case the caller logs
    /// and treats as no-stamp).
    fn retest_tolerance(
        &self,
        break_at: chrono::DateTime<chrono::Utc>,
        candle: &Candle,
        window: &[Candle],
        plan: &crate::plan::TradePlan,
    ) -> Option<f64> {
        let bars_since_break = window
            .iter()
            .filter(|c| c.time > break_at && c.time <= candle.time)
            .count();
        if bars_since_break <= 1 {
            return Some(0.0);
        }
        let atr = wilder_atr(window, atr_length_for(plan.granularity))?;
        Some((bars_since_break as f64 - 1.0) * plan.retest_atr_step * atr)
    }
}

/// A retest cross, loosened by a near-side `tol` (price units) — a faithful port
/// of v1's `retest_crossed`. Identical to [`eval_trigger`] except the *intrabar
/// directional* arm accepts a wick that comes **within `tol`** of the line on
/// the retest side, rather than requiring it to reach/pierce the line. `tol ==
/// 0.0` is exactly [`eval_trigger`] (the first-bar / no-decay case). `OnClose`
/// and `Either` keep their exact semantics (the closeness tolerance loosens only
/// the intrabar directional arm).
///
/// In practice a v2 retest rule always builds a [`Trigger::TrendlineCross`] (see
/// [`Retest::trigger_for`]), so the trendline arm is the one taken; the full
/// match is kept for faithfulness and totality.
fn retest_crossed(
    trigger: &Trigger,
    candle: &Candle,
    prev_close: Option<f64>,
    window: &[Candle],
    buffer_pct: f64,
    tol: f64,
) -> bool {
    // With no slack, defer to the shared evaluator (single source of truth for
    // the strict cross, buffer, and OnClose/Either arms).
    if tol <= 0.0 {
        return eval_trigger(trigger, candle, prev_close, window, buffer_pct);
    }
    // Resolve the crossed level. Only trendline / horizontal / price-value
    // crosses have a level; anything else falls back to the strict evaluator.
    let (level, dir, bar) = match trigger {
        Trigger::HorizontalCross { level, dir, bar }
        | Trigger::PriceValueCross { level, dir, bar } => (*level, *dir, *bar),
        Trigger::TrendlineCross {
            a,
            b,
            extend_forward,
            bar_seconds,
            dir,
            bar,
        } => {
            let Some(level) = line_price_at(a, b, candle, *extend_forward, *bar_seconds, window)
            else {
                return false;
            };
            (level, *dir, *bar)
        }
        _ => return eval_trigger(trigger, candle, prev_close, window, buffer_pct),
    };
    // The closeness tolerance only loosens the *intrabar directional* arm; an
    // OnClose or Either retest keeps its exact semantics.
    match bar {
        BarEvent::Intrabar => match dir {
            // Near-side loosening: the wick only has to come within `tol` of the
            // line, not reach it. Long retest (`Down`): low dips to `level + tol`
            // or lower. Short (`Up`): high rises to `level - tol` or higher.
            CrossDir::Down => candle.l <= level + tol,
            CrossDir::Up => candle.h >= level - tol,
            CrossDir::Either => candle.l <= level + tol && candle.h >= level - tol,
        },
        BarEvent::OnClose => level_crossed(level, dir, bar, candle, prev_close, buffer_pct),
    }
}
