//! [`Invalidate`] — the invalidation-cap rule, **fact-based & pure**.
//!
//! The first rule to consume a [`PriceLevel`](crate::PriceLevel) rather than a
//! [`Line`](crate::Line), and the first to emit the terminal
//! [`Effect::Invalidate`]. On a genuine cross of its cap it **retires the plan**:
//! the driver stamps the plan-scoped `invalidated` fact and the enter (its second
//! fire-once guard) never places again. Like the other rules it only *reads* the
//! [`Facts`](crate::facts::Facts) blackboard and mutates nothing — the retire
//! leaves as an [`Effect`] for the driver to apply.
//!
//! # Cap = a horizontal level, crossed with NO projection
//!
//! Unlike [`BreakAndClose`](super::BreakAndClose) / [`Retest`](super::Retest) —
//! which resolve a sloped neckline's price by bar-index projection — an
//! invalidation cap is a single horizontal price. It uses the no-projection
//! [`eval_level`](crate::cross::eval_level) path (the v1
//! `HorizontalCross` arm), so the whole `line_price_at` machinery is skipped. The
//! cap price comes straight from [`TradePlan::level_typed`](crate::TradePlan::level_typed).
//!
//! # What it reproduces from the proven cross logic
//!
//! 1. **Fire-once (plan-scoped)** — if the plan is already retired
//!    (`(PLAN_SCOPE, "invalidated")` set), this rule is done. One invalidation
//!    retires the whole plan, so the guard is plan-scoped, not per-line: a
//!    `too_high` cross and a later `too_low` cross don't both need to fire.
//! 2. **`last_close` bookkeeping before the gate** — an `OnClose` cap measures this
//!    close against the rule's prior close, held as rule-private scratch keyed
//!    `(rule_id, "last_close")`. Emitted on every past-fire-once tick so a genuine
//!    cross on the next clean bar is measured correctly. Same subtlety as
//!    break-and-close; only `OnClose` caps track it.
//! 3. **Spread-hour suppression gates the RETIRE, not the bookkeeping** — a cap
//!    cross on a learned spread hour is a liquidity-vacuum wick, not a genuine
//!    invalidation: don't retire (but the `last_close` scratch still records).
//!
//! # `StopNextEntry`-only
//!
//! The retire *blocks the entry*; it never closes a position. In this slice the
//! enter is single-shot with no open position to manage, so retirement is exactly
//! entry-blocking — faithful to the `veto_close_only_when_thesis_invalidated`
//! rule. When position management lands, a separate close-effect handles an open
//! trade; this rule stays entry-blocking.

use std::marker::PhantomData;

use trade_control_core::broker::Candle;
use trade_control_core::trade_plan::BarEvent;

use crate::PlanRule;
use crate::cross::eval_level;
use crate::effect::Effect;
use crate::facts::{FactKind, FactValue, Invalidated, LastClose, LineName, PLAN_SCOPE};
use crate::rule::Rule;
use crate::world::World;

/// The invalidation-cap rule, bound to a v2 [`PlanRule`] and a compile-time cap
/// `L` (the [`PriceLevel`](crate::PriceLevel) name it targets — `TooHigh` or
/// `TooLow`). Borrowed so instantiating it per tick is free; `L` is a zero-size
/// [`PhantomData`] marker.
pub struct Invalidate<'r, L: LineName> {
    /// The rule this wraps.
    pub rule: &'r PlanRule,
    _level: PhantomData<L>,
}

impl<'r, L: LineName> Invalidate<'r, L> {
    /// Wrap an invalidation [`PlanRule`] targeting cap `L`.
    pub fn new(rule: &'r PlanRule) -> Self {
        Self {
            rule,
            _level: PhantomData,
        }
    }
}

impl<L: LineName> Rule for Invalidate<'_, L> {
    fn rule_id(&self) -> &str {
        &self.rule.id
    }

    fn tick(&self, w: &World) -> Vec<Effect> {
        // Fire-once, plan-scoped: once the plan is retired, every invalidation rule
        // is done. Reading the retire fact to check "already done" is fine — a pure
        // rule may read facts, it just can't write them.
        if w.facts.is_set_named(PLAN_SCOPE, Invalidated::NAME) {
            return Vec::new();
        }

        // The current closed bar (last of the window). Absent only for an empty
        // window, which the driver already guards.
        let Some(candle) = w.current() else {
            return Vec::new();
        };

        // The cap `L` this rule targets must exist in the plan's levels.
        let Some(level) = w.plan.level_typed::<L>() else {
            return Vec::new();
        };

        let mut effects = Vec::new();

        // `last_close` bookkeeping BEFORE the gate — emitted on every past-fire-once
        // tick (firing or not, spread-hour or not), as a rule-private scratch write
        // for the driver to apply. Only `OnClose` caps track it.
        if uses_close(self.rule.bar) {
            effects.push(Effect::WriteScratch {
                rule_id: self.rule.id.clone(),
                kind: LastClose::NAME.to_string(),
                value: FactValue::Num(candle.c),
            });
        }

        let prev_close = w.facts.num_scratch::<LastClose>(&self.rule.id);
        let hit = eval_level(
            level.price,
            self.rule.dir,
            self.rule.bar,
            candle,
            prev_close,
            w.plan.cross_buffer_pct,
        );

        if !self.should_retire(hit, candle) {
            return effects;
        }

        // Genuine cap cross: retire the plan. The driver stamps the plan-scoped
        // `invalidated` fact (so the enter's guard sees it) and returns this effect
        // to the caller as the terminal signal.
        effects.push(Effect::Invalidate {
            rule_id: self.rule.id.clone(),
        });
        effects
    }
}

impl<L: LineName> Invalidate<'_, L> {
    /// Does a raw cap cross `hit` translate into a retire? Suppressed on a
    /// spread-hour bar (the gate applies to the retire only, never to the
    /// `last_close` bookkeeping — that's already been emitted by the caller). Same
    /// spread-hour subtlety as [`BreakAndClose`](super::BreakAndClose).
    fn should_retire(&self, hit: bool, candle: &Candle) -> bool {
        hit && !trade_control_core::spread_blackout::is_spread_hour(
            &self.rule.intent.instrument,
            candle.time,
        )
    }
}

/// Whether a cap's bar-event mode reads the prior close (so `last_close` must be
/// tracked). Mirrors [`trigger_uses_close`](crate::cross::trigger_uses_close) for a
/// bare [`BarEvent`] — an `OnClose` cap needs the prior close; an intrabar one
/// does not.
fn uses_close(bar: BarEvent) -> bool {
    matches!(bar, BarEvent::OnClose)
}
