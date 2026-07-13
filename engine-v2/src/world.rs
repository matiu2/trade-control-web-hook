//! [`World`] — the per-tick context the driver hands each rule's
//! [`tick`](crate::rule::Rule::tick).
//!
//! v2 shape (fact-based): there is **no `PlanState` and no `phase`**. A rule
//! reads the [`Facts`] blackboard (facts other rules wrote) and the v2
//! [`TradePlan`] (its lines + rules + buffer), and writes its own facts back.
//! The driver rebuilds a fresh `World` per candle; every borrow is tied to that
//! one tick by the lifetime `'a`.

use chrono::{DateTime, Utc};

use trade_control_core::broker::Candle;

use crate::facts::Facts;
use crate::plan::TradePlan;

/// The per-candle context the driver passes to each rule.
pub struct World<'a> {
    /// The tick's wall-clock instant. A candle-driven cross (break-and-close)
    /// ignores it; the field is here for the control/time rules of later slices.
    pub now: DateTime<Utc>,
    /// The candle being processed. `None` on a sub-bar (mid-candle) tick —
    /// unused this slice (break-and-close only runs on a closed bar).
    pub candle: Option<&'a Candle>,
    /// The ascending detector window used to resolve a sloped line's level in
    /// bar-index space. Unused for a horizontal line.
    pub window: &'a [Candle],
    /// The fact blackboard — read facts other rules wrote, write your own.
    pub facts: &'a mut Facts,
    /// The v2 plan — for its lines, rules, and `cross_buffer_pct`.
    pub plan: &'a TradePlan,
}
