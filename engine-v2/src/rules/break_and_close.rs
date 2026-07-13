//! [`BreakAndClose`] — the break-and-close prep rule.
//!
//! A **faithful port** of the old engine's `evaluate_break_and_close` and the
//! helpers it calls (`fire_rule`, `record_last_close`). On fire it latches its
//! `rule_id` in `state.fired`, stamps `state.break_close_at` to the candle's
//! open-time, dispatches its prep intent, and advances the phase to
//! `AwaitEntry`. Once latched it never re-stamps (the multi-enter re-cross
//! guard — old-engine's explicit latch check).
//!
//! # Where the old engine's driver logic lands here
//!
//! The old `evaluate_break_and_close` did three things this rule must reproduce
//! exactly:
//!
//! 1. **Latch check** — `if state.fired.contains(&rule.rule_id) { return }`.
//!    Preserved: a fired rule produces no effect and mutates nothing.
//! 2. **Fire decision** — `fire_rule(...)`: evaluate the trigger against the
//!    prior close, **record `last_close` before** the spread-hour gate, then
//!    gate the *fire* (not the bookkeeping) on the spread hour. Ported verbatim.
//! 3. **On fire** — insert the latch, stamp `break_close_at = candle.time`, push
//!    the fired intent, set phase `AwaitEntry`.
//!
//! The old engine's "no break-and-close rule but we're in its phase → advance
//! immediately" defensive arm is the **driver's** job here (there's no rule to
//! tick), and the driver handles it — see `driver.rs`.

use trade_control_core::broker::Candle;
use trade_control_core::plan_state::{Phase, PlanState};
use trade_control_core::trade_plan::{ConditionRule, Trigger};

use crate::cross::{eval_trigger, trigger_uses_close};
use crate::effect::Effect;
use crate::rule::Rule;
use crate::world::World;

/// The break-and-close prep, bound to the plan's `03-prep-break-and-close`
/// [`ConditionRule`]. Borrowed (not owned) so instantiating it per tick is free
/// and it reads the same rule the plan carries.
pub struct BreakAndClose<'r> {
    /// The break-and-close rule this wraps.
    pub rule: &'r ConditionRule,
}

impl<'r> BreakAndClose<'r> {
    /// Wrap a break-and-close `ConditionRule`.
    pub fn new(rule: &'r ConditionRule) -> Self {
        Self { rule }
    }
}

impl Rule for BreakAndClose<'_> {
    fn rule_id(&self) -> &str {
        &self.rule.rule_id
    }

    fn tick(&self, w: &mut World) -> Vec<Effect> {
        // The break-and-close prep is `FireMode::Once`: it stamps the lookback
        // start exactly once and then "dies". Honour the latch explicitly —
        // once fired, never re-stamp (the multi-enter re-cross guard, old
        // engine's replay trade 071).
        if w.state.fired.contains(&self.rule.rule_id) {
            return Vec::new();
        }

        // The driver only ticks a real closed bar this slice; `candle` is
        // always `Some`. If a future sub-bar tick ever reaches here with `None`,
        // there is nothing to break-and-close on.
        let Some(candle) = w.candle else {
            return Vec::new();
        };

        if !fire_rule(
            self.rule,
            w.state,
            candle,
            w.window,
            w.plan.cross_buffer_pct,
        ) {
            return Vec::new();
        }

        // On fire: latch, stamp the lookback start, advance the phase, and
        // dispatch the prep intent (the driver folds the returned effect into
        // `PlanEval::fired`).
        w.state.fired.insert(self.rule.rule_id.clone());
        w.state.break_close_at = Some(candle.time);
        w.state.phase = Phase::AwaitEntry;

        vec![Effect::Fire(crate::rule::fired_intent(self.rule, candle))]
    }
}

/// Fire a rule against a candle: evaluate its trigger (updating the rule's
/// `last_close` memory) and return whether it fired this bar. Port of the old
/// engine's `fire_rule`.
///
/// Crucially, `last_close` is persisted **before** the spread-hour gate: a
/// spread-hour bar still seeds `last_close` so a genuine OnClose cross on the
/// next clean bar is measured against it — the suppression gates the *fire*
/// decision, not the bookkeeping.
fn fire_rule(
    rule: &ConditionRule,
    state: &mut PlanState,
    candle: &Candle,
    window: &[Candle],
    buffer_pct: f64,
) -> bool {
    let prev_close = state.last_close.get(&rule.rule_id).copied();
    let hit = eval_trigger(&rule.trigger, candle, prev_close, window, buffer_pct);
    record_last_close(&rule.rule_id, &rule.trigger, candle, state);
    if hit
        && trade_control_core::spread_blackout::is_spread_hour(&rule.intent.instrument, candle.time)
    {
        return false;
    }
    hit
}

/// Persist this candle's close as the rule's `last_close` so an `OnClose` cross
/// can be detected against it next bar. Port of the old engine's
/// `record_last_close`.
fn record_last_close(rule_id: &str, trigger: &Trigger, candle: &Candle, state: &mut PlanState) {
    if trigger_uses_close(trigger) {
        state.last_close.insert(rule_id.to_string(), candle.c);
    }
}
