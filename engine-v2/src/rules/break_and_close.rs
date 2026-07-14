//! [`BreakAndClose`] — the break-and-close prep rule, **fact-based & pure**.
//!
//! This is the v2 rewrite of the first prep. It only *reads* the
//! [`Facts`](crate::facts::Facts) blackboard — there is **no `Phase`** and no
//! `PlanState`, and it **mutates nothing**. Everything it produces leaves as an
//! [`Effect`] for the driver to apply: on a genuine close through its line it
//! returns [`Effect::WriteFact`] stamping `(line, "break_close") =
//! At(candle.time)` **and** [`Effect::Fire`]; its cross bookkeeping leaves as an
//! [`Effect::WriteScratch`].
//!
//! # What it reproduces from the proven v1 logic
//!
//! 1. **Fire-once** — if `(line, "break_close")` is already set, this rule is done:
//!    do nothing and emit nothing. (v1: `state.fired.contains(rule_id)`.) Reading
//!    the fact to check "already done" is fine — only *writing* moves to effects.
//! 2. **`last_close` bookkeeping before the gate** — an `OnClose` cross measures
//!    this candle's close against the rule's *prior* close. That prior close is
//!    **rule-private scratch**, keyed `(rule_id, "last_close")` — not a shared
//!    `(line, kind)` fact (see [`facts`](crate::facts)). The rule emits a
//!    [`WriteScratch`](Effect::WriteScratch) for it on **every** tick that gets
//!    past the fire-once check (even a spread-hour bar and even a non-firing bar),
//!    so a genuine cross on the next clean bar is measured correctly. This is v1's
//!    "record `last_close` before the spread-hour gate" subtlety, expressed as a
//!    scratch effect instead of an in-place `PlanState.last_close` mutation.
//! 3. **Spread-hour suppression gates the FIRE, not the bookkeeping** — a hit on
//!    a spread-hour bar still emits the `last_close` scratch write but does not
//!    fire or stamp `break_close`.
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
use crate::facts::FactValue;
use crate::plan::{Line, PlanRule};
use crate::rule::Rule;
use crate::world::World;

/// Shared-fact kind for the break-and-close stamp (keyed `(line, kind)`).
const KIND_BREAK_CLOSE: &str = "break_close";
/// Rule-private scratch kind for the prior-close bookkeeping (keyed
/// `(rule_id, kind)`).
const SCRATCH_LAST_CLOSE: &str = "last_close";

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

    fn tick(&self, w: &World) -> Vec<Effect> {
        // Fire-once: once the break-and-close fact is set, this rule is done — it
        // never re-stamps (v1's multi-enter re-cross guard). Reading to check
        // "already done" is fine; a pure rule may read facts, it just can't write
        // them.
        if w.facts.is_set(&self.rule.line, KIND_BREAK_CLOSE) {
            return Vec::new();
        }

        // The current closed bar (last of the window). Absent only for an empty
        // window, which the driver already guards.
        let Some(candle) = w.current() else {
            return Vec::new();
        };

        // The line this rule references must exist in the plan.
        let Some(line) = w.plan.line(&self.rule.line) else {
            return Vec::new();
        };

        let trigger = self.trigger_for(line, w.plan.granularity.seconds());
        let prev_close = w.facts.num_scratch(&self.rule.id, SCRATCH_LAST_CLOSE);
        let hit = eval_trigger(
            &trigger,
            candle,
            prev_close,
            w.window,
            w.plan.cross_buffer_pct,
        );

        let mut effects = Vec::new();

        // `last_close` bookkeeping BEFORE the gate — emitted on every tick that
        // passed the fire-once check (firing or not), as a rule-private scratch
        // write for the driver to apply. Only `OnClose` triggers track it.
        if trigger_uses_close(&trigger) {
            effects.push(Effect::WriteScratch {
                rule_id: self.rule.id.clone(),
                kind: SCRATCH_LAST_CLOSE.to_string(),
                value: FactValue::Num(candle.c),
            });
        }

        if !self.should_fire(hit, candle) {
            return effects;
        }

        // Genuine cross: stamp the break-close shared fact AND dispatch the prep
        // intent — both as effects, applied by the driver.
        effects.push(Effect::WriteFact {
            line: self.rule.line.clone(),
            kind: KIND_BREAK_CLOSE.to_string(),
            value: FactValue::At(candle.time),
        });
        effects.push(Effect::Fire(Box::new(crate::rule::fired_intent(
            self.rule, candle,
        ))));
        effects
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
            // A neckline always projects forward past its second anchor.
            extend_forward: true,
            bar_seconds,
            dir: self.rule.dir,
            bar: self.rule.bar,
        }
    }

    /// Does a raw cross `hit` translate into a fire? Suppressed on a spread-hour
    /// bar (the gate applies to the fire only, never to the `last_close`
    /// bookkeeping — that's already been emitted by the caller).
    fn should_fire(&self, hit: bool, candle: &Candle) -> bool {
        hit && !trade_control_core::spread_blackout::is_spread_hour(
            &self.rule.intent.instrument,
            candle.time,
        )
    }
}
