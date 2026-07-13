//! engine-v2 — a rule-trait + driver-loop reimplementation of the trade-plan
//! engine, built alongside the existing `trade-control-engine` (the old
//! `evaluate_plan`) and proven byte-identical to it, one rule at a time, via a
//! parity harness.
//!
//! # Why this crate exists
//!
//! See `SCOPING-rule-based-engine.md`. In short: today every trade-rule is
//! implemented twice (worker dispatch + replay `simulate_fill`) and kept in
//! sync by discipline; that "two-implementation tax" is paid on every future
//! rule. engine-v2 collapses the interpretation into **one** driver loop that
//! ticks a `Vec<Box<dyn Rule>>` derived from the plan, so replay and live can
//! only differ inside the leaf `Broker` / `Storage` impls — not in strategy
//! interpretation.
//!
//! # This slice (slice 1)
//!
//! The **minimal** proof of the pattern with the one rule that needs no broker
//! I/O — **break-and-close**. It ships:
//!
//! - the [`Rule`] trait and the [`Effect`] enum (minimal surface),
//! - the per-candle [`World`] the driver hands each rule,
//! - the [`drive`] driver loop returning a [`PlanEval`] (same output type as
//!   the old engine, so the parity harness can diff directly), and
//! - [`BreakAndClose`], the first `Rule`, a faithful port of the old engine's
//!   `evaluate_break_and_close` + its helpers.
//!
//! No `Broker` / `Storage` traits, no fill simulation, no news/control/guard/
//! entry rules, no ordering system — those are later slices. The driver holds
//! [`PlanState`] in-memory, exactly as the old engine does.
//!
//! # Types
//!
//! Every domain type is reused verbatim from [`trade_control_core`] — the
//! driver returns a [`PlanEval`] carrying [`FiredIntent`]s and the advanced
//! [`PlanState`]. Nothing is redefined here.

// Re-export the shared surface, mirroring how the old `engine` crate does it, so
// downstream callers can name everything through `trade_control_engine_v2`.
pub use trade_control_core::broker::{Candle, Granularity};
pub use trade_control_core::intent::{Action, Intent};
pub use trade_control_core::plan_eval::{FiredIntent, PlanEval};
pub use trade_control_core::plan_state::{Phase, PlanState};
pub use trade_control_core::trade_plan::{
    BarEvent, ConditionRule, CrossDir, FireMode, LinePoint, TradePlan, Trigger,
};

mod cross;
mod driver;
mod effect;
mod rule;
mod world;

mod rules;

pub use driver::{drive, initial_phase, seed_plan_state};
pub use effect::Effect;
pub use rule::Rule;
pub use rules::BreakAndClose;
pub use world::World;
