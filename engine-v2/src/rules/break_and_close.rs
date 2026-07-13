//! [`BreakAndClose`] — the break-and-close prep rule, **fact-based**.
//!
//! This is the v2 rewrite of the first prep. It reads/writes only the
//! [`Facts`](crate::facts::Facts) blackboard — there is **no `Phase`** and no
//! `PlanState`. On a genuine close through its line it writes the fact
//! `(line, "break_close") = At(candle.time)` and emits [`Effect::Fire`].
//!
//! # What it reproduces from the proven v1 logic
//!
//! 1. **Latch** — if `(line, "break_close")` is already set, do nothing and
//!    write nothing. (v1: `state.fired.contains(rule_id)`.)
//! 2. **`last_close` bookkeeping before the gate** — an `OnClose` cross measures
//!    this candle's close against the rule's *prior* close. That prior close is
//!    itself a fact: `(line, "last_close") = Num(close)`. It is recorded on
//!    **every** tick (even a spread-hour bar and even a non-firing bar), so a
//!    genuine cross on the next clean bar is measured correctly. This is v1's
//!    "record `last_close` before the spread-hour gate" subtlety, expressed as a
//!    fact instead of `PlanState.last_close`.
//! 3. **Spread-hour suppression gates the FIRE, not the bookkeeping** — a hit on
//!    a spread-hour bar records `last_close` but does not fire.
//!
//! # Cross evaluation
//!
//! The v2 [`Line`] + the rule's `bar`/`dir` are assembled into a v1
//! [`Trigger::TrendlineCross`] purely to feed the **proven, reused** `cross.rs`
//! projection (bar-index interpolation, gap collapse, `extend_forward`). A
//! horizontal line is just a trendline whose two anchor prices are equal, so one
//! code path covers both. Nothing in `cross.rs` was changed.

use trade_control_core::broker::Candle;
use trade_control_core::trade_plan::Trigger;

use crate::cross::{eval_trigger, trigger_uses_close};
use crate::effect::Effect;
use crate::facts::{FactValue, Facts};
use crate::plan::{Line, PlanRule};
use crate::rule::Rule;
use crate::world::World;

/// Fact kind for the break-and-close stamp.
const KIND_BREAK_CLOSE: &str = "break_close";
/// Fact kind for the per-rule prior-close bookkeeping.
const KIND_LAST_CLOSE: &str = "last_close";

/// The break-and-close prep, bound to a v2 [`PlanRule`]. Borrowed so
/// instantiating it per tick is free.
pub struct BreakAndClose<'r> {
    /// The rule this wraps.
    pub rule: &'r PlanRule,
}

impl<'r> BreakAndClose<'r> {
    /// Wrap a break-and-close [`PlanRule`].
    pub fn new(rule: &'r PlanRule) -> Self {
        Self { rule }
    }
}

impl Rule for BreakAndClose<'_> {
    fn rule_id(&self) -> &str {
        &self.rule.id
    }

    fn tick(&self, w: &mut World) -> Vec<Effect> {
        // Latch: once the break-and-close fact is set, this rule is done — it
        // never re-stamps (v1's multi-enter re-cross guard).
        if w.facts.is_set(&self.rule.line, KIND_BREAK_CLOSE) {
            return Vec::new();
        }

        // Break-and-close only runs on a real closed bar this slice.
        let Some(candle) = w.candle else {
            return Vec::new();
        };

        // The line this rule references must exist in the plan.
        let Some(line) = w.plan.line(&self.rule.line) else {
            return Vec::new();
        };

        let trigger = self.trigger_for(line, w.plan.granularity.seconds());

        if !self.fire(&trigger, candle, w.window, w.plan.cross_buffer_pct, w.facts) {
            return Vec::new();
        }

        // Genuine cross: stamp the break-close fact and dispatch the prep intent.
        w.facts.set(
            &self.rule.line,
            KIND_BREAK_CLOSE,
            FactValue::At(candle.time),
        );
        vec![Effect::Fire(crate::rule::fired_intent(self.rule, candle))]
    }
}

impl BreakAndClose<'_> {
    /// Assemble the v1 [`Trigger::TrendlineCross`] that `cross.rs` evaluates,
    /// from the v2 line + the rule's bar/dir. A horizontal line falls out as
    /// `a.price == b.price`.
    fn trigger_for(&self, line: &Line, bar_seconds: i64) -> Trigger {
        Trigger::TrendlineCross {
            a: line.a,
            b: line.b,
            extend_forward: line.extend_forward,
            bar_seconds,
            dir: self.rule.dir,
            bar: self.rule.bar,
        }
    }

    /// Evaluate the trigger for one candle: record the `last_close` fact
    /// (before the gate), then gate the *fire* on the spread hour. Returns
    /// whether it fired.
    fn fire(
        &self,
        trigger: &Trigger,
        candle: &Candle,
        window: &[Candle],
        buffer_pct: f64,
        facts: &mut Facts,
    ) -> bool {
        let prev_close = facts.num(&self.rule.line, KIND_LAST_CLOSE);
        let hit = eval_trigger(trigger, candle, prev_close, window, buffer_pct);
        self.record_last_close(trigger, candle, facts);
        if hit
            && trade_control_core::spread_blackout::is_spread_hour(
                &self.rule.intent.instrument,
                candle.time,
            )
        {
            return false;
        }
        hit
    }

    /// Persist this candle's close as the rule's `last_close` fact, so an
    /// `OnClose` cross can be detected against it next bar (only `OnClose`
    /// triggers track it).
    fn record_last_close(&self, trigger: &Trigger, candle: &Candle, facts: &mut Facts) {
        if trigger_uses_close(trigger) {
            facts.set(&self.rule.line, KIND_LAST_CLOSE, FactValue::Num(candle.c));
        }
    }
}
