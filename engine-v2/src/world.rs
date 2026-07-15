//! [`World`] — the per-tick context the driver hands each rule's
//! [`tick`](crate::rule::Rule::tick).
//!
//! v2 shape (fact-based, **pure rules**): there is **no `PlanState` and no
//! `phase`**. A rule *reads* the [`Facts`] blackboard (facts other rules wrote)
//! and the v2 [`TradePlan`] (its lines + rules + buffer), but it does **not**
//! write anything through the `World` — `facts` is a shared `&Facts`, read-only.
//! Every output (fires and fact/scratch writes) leaves the rule as an
//! [`Effect`](crate::effect::Effect); the driver applies those. The driver
//! rebuilds a fresh `World` per candle; every borrow is tied to that one tick by
//! the lifetime `'a`.

use chrono::{DateTime, Utc};

use trade_control_core::broker::Candle;

use crate::TradePlan;
use crate::facts::Facts;

/// The per-candle context the driver passes to each rule.
pub struct World<'a> {
    /// The tick's wall-clock instant. A candle-driven cross (break-and-close)
    /// ignores it; the field is here for the control/time rules of later slices.
    /// A rule derives bar-relative properties (e.g. "is this bar stale", "are we
    /// mid-bar") from `now` vs [`current`](World::current)`.time` — the driver
    /// stamps no such flag (that would smuggle mode-branching into the rules).
    pub now: DateTime<Utc>,
    /// The ascending detector window ending at the bar under evaluation.
    /// [`current`](World::current) is `window.last()` — the closed bar this tick
    /// processes. Also what a sloped line resolves its level against in
    /// bar-index space (unused for a horizontal line).
    pub window: &'a [Candle],
    /// The fact blackboard — **read-only**. Read facts other rules wrote (and
    /// your own scratch); to write, return a
    /// [`WriteFact`](crate::effect::Effect::WriteFact) /
    /// [`WriteScratch`](crate::effect::Effect::WriteScratch) effect for the
    /// driver to apply.
    pub facts: &'a Facts,
    /// The v2 plan — for its lines, rules, and `cross_buffer_pct`.
    pub plan: &'a TradePlan,
}

impl<'a> World<'a> {
    /// The current bar this tick processes — the last of the [`window`](World::window).
    /// `None` only if the window is empty (the driver returns early in that
    /// case, so rules receiving a `World` can rely on `Some`).
    pub fn current(&self) -> Option<&'a Candle> {
        self.window.last()
    }
}
