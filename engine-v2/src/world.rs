//! [`World`] — what the driver builds per candle and hands to each rule's
//! [`tick`](crate::rule::Rule::tick).
//!
//! It bundles the current instant, the candle being processed (the detector
//! window it belongs to), the plan (for its rule list and plan-level buffer),
//! and a **mutable** borrow of the fact blackboard [`PlanState`] — the rule
//! reads facts other rules wrote (`break_close_at`, `fired`, `last_close`) and
//! writes its own back. For slice 1 the state *is* the blackboard, exactly as
//! the old engine mutates `PlanState` in place.

use chrono::{DateTime, Utc};

use trade_control_core::broker::Candle;
use trade_control_core::plan_state::PlanState;
use trade_control_core::trade_plan::TradePlan;

/// The per-candle context the driver passes to each rule's
/// [`tick`](crate::rule::Rule::tick).
///
/// The lifetime `'a` ties every borrow to the single tick — the driver rebuilds
/// a fresh `World` per candle. `state` is `&mut` because a rule mutates facts on
/// it (this slice: `break_close_at`, the fire latch, the phase, `last_close`).
pub struct World<'a> {
    /// The tick's wall-clock instant. Break-and-close ignores it (it's a
    /// candle-driven cross), but the field is here for parity with the driver's
    /// signature and for the control/time rules of later slices.
    pub now: DateTime<Utc>,
    /// The candle being processed this tick. `None` is reserved for a sub-bar
    /// (mid-candle) tick — unused this slice; break-and-close only runs on a
    /// real closed bar.
    pub candle: Option<&'a Candle>,
    /// The detector window — the ascending back-window of closed candles used
    /// to resolve a `TrendlineCross`'s level in bar-index space. For a
    /// horizontal break-and-close it's unused; the driver passes the same
    /// window the old engine's `detector_window` carries.
    pub window: &'a [Candle],
    /// The fact blackboard. Rules read facts other rules wrote and write their
    /// own here. Mutated in place this slice.
    pub state: &'a mut PlanState,
    /// The plan — for its rule list and plan-level `cross_buffer_pct`.
    pub plan: &'a TradePlan,
}
