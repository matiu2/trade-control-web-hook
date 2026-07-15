//! [`Expiry`] — the trade-expiry rule, **fact-based & pure**.
//!
//! The first rule to consume a [`TimeMarker`](crate::TimeMarker): when the bar
//! reaches the plan's expiry time it **retires the plan**, reusing 4c's terminal
//! [`Effect::Invalidate`] wholesale (an expiry *is* a retirement — the plan is
//! dead, the enter must not place). Like the other rules it only *reads* the
//! [`Facts`](crate::facts::Facts) blackboard and mutates nothing.
//!
//! # A time check, not a price cross — and no spread-hour / `last_close`
//!
//! Unlike [`BreakAndClose`](super::BreakAndClose) / [`Invalidate`](super::Invalidate)
//! this has no price geometry: it uses [`eval_time`](crate::cross::eval_time)
//! (`candle.time >= marker`, the v1 `TimeReached` arm). Two consequences:
//!
//! - **No `last_close` bookkeeping.** There is no `OnClose`-vs-intrabar distinction
//!   for a time check — "has the bar reached time T?" needs no prior close.
//! - **No spread-hour suppression.** A spread hour is a *price* liquidity vacuum;
//!   it has no bearing on whether wall-clock time has passed. Expiry fires on the
//!   spread-hour bar exactly as on any other. (This mirrors v1: `trade-expiry` is a
//!   spine `TimeReached` fire, ungated by the spread-hour rubbish-candle logic that
//!   only guards price crosses.)
//!
//! # `candle.time`, not the tick's `now`
//!
//! [`eval_time`](crate::cross::eval_time) compares the **bar's** time, matching
//! v1's spine `trade-expiry` ("fires on the bar whose open passes expiry"). See
//! that function's docs for why the spine uses `candle.time` while v1's *control*
//! `TimeReached` (pause/news, not yet in engine-v2) uses wall-clock `now`.
//!
//! # Fire-once + `StopNextEntry`-only
//!
//! Fire-once on the plan-scoped retire fact (an expired plan is retired, the same
//! terminal state an invalidation cap produces — so one guard covers both). It is
//! `StopNextEntry`-only: retirement blocks the enter, it never closes a position
//! (single-shot, no position management yet).

use std::marker::PhantomData;

use crate::PlanRule;
use crate::cross::eval_time;
use crate::effect::Effect;
use crate::facts::{FactKind, Invalidated, LineName, PLAN_SCOPE};
use crate::rule::Rule;
use crate::world::World;

/// The trade-expiry rule, bound to a v2 [`PlanRule`] and a compile-time marker
/// `L` (the [`TimeMarker`](crate::TimeMarker) name it targets — `Expiry` today).
/// Borrowed so instantiating it per tick is free; `L` is a zero-size
/// [`PhantomData`] marker.
pub struct Expiry<'r, L: LineName> {
    /// The rule this wraps.
    pub rule: &'r PlanRule,
    _marker: PhantomData<L>,
}

impl<'r, L: LineName> Expiry<'r, L> {
    /// Wrap an expiry [`PlanRule`] targeting marker `L`.
    pub fn new(rule: &'r PlanRule) -> Self {
        Self {
            rule,
            _marker: PhantomData,
        }
    }
}

impl<L: LineName> Rule for Expiry<'_, L> {
    fn rule_id(&self) -> &str {
        &self.rule.id
    }

    fn tick(&self, w: &World) -> Vec<Effect> {
        // Fire-once, plan-scoped: once the plan is retired (by this expiry or an
        // invalidation cap), this rule is done.
        if w.facts.is_set_named(PLAN_SCOPE, Invalidated::NAME) {
            return Vec::new();
        }

        // The current closed bar (last of the window). Absent only for an empty
        // window, which the driver already guards.
        let Some(candle) = w.current() else {
            return Vec::new();
        };

        // The marker `L` this rule targets must exist in the plan's markers.
        let Some(marker) = w.plan.marker_typed::<L>() else {
            return Vec::new();
        };

        // Has the bar reached the expiry time? (No price, no spread-hour gate, no
        // `last_close` — see the module docs.)
        if !eval_time(marker.at_epoch, candle) {
            return Vec::new();
        }

        // Expired: retire the plan. The driver stamps the plan-scoped `invalidated`
        // fact (so the enter's guard sees it) and returns this effect to the caller
        // as the terminal signal — reusing 4c's retire path.
        vec![Effect::Invalidate {
            rule_id: self.rule.id.clone(),
        }]
    }
}
