//! engine-v2 — a clean-slate, **fact-based** rule engine for the trade-plan
//! system, built alongside the untouched live v1 engine (`trade-control-engine`)
//! and judged by trade profitability, **not** byte-parity with v1.
//!
//! See `SCOPING-rule-based-engine.md`. The design in one line: **rules
//! communicate through facts in a shared blackboard, not through a central
//! `Phase` state machine.** Facts are keyed by `(line, kind)` and one rule
//! (break-and-close) writes a fact a later rule (retest, enter) reads. There is
//! no sequential spine; each rule, when ticked, reads the facts it needs and
//! decides for itself.
//!
//! # This slice (slice 1)
//!
//! The minimal fact-based foundation the **one** rule that needs no broker I/O
//! demands — break-and-close on a named line. It ships:
//!
//! - a v2 [`TradePlan`] / [`Line`] / [`PlanRule`] model — lines (named geometry)
//!   separated from rules (behaviour referencing a line by name),
//! - the [`Facts`] blackboard ([`FactValue`], keyed by `(line, kind)`),
//! - a v2 [`World`] (no `PlanState`, no phase — holds `&mut Facts` + the v2
//!   plan),
//! - the [`Rule`] behaviour trait and the [`Effect`] enum (`Fire` only),
//! - [`BreakAndClose`], the first rule, reusing the proven `cross.rs`
//!   line-projection, and
//! - a minimal [`drive`] loop that ticks the plan's break-and-close rules and
//!   collects effects.
//!
//! No `Broker`/`Storage`, no entry/retest/news rules, no ordering system — those
//! are later slices, added on demand as each rule requires them.
//!
//! # The two "Rule" concepts
//!
//! [`PlanRule`] is plan **data** (serializable wire format); [`Rule`] is
//! **behaviour** (the trait the driver ticks). See [`plan`] for the naming
//! rationale.

// Domain primitives reused verbatim from `trade_control_core`.
pub use trade_control_core::broker::{Candle, Granularity};
pub use trade_control_core::intent::{Action, Direction, Intent};
pub use trade_control_core::plan_eval::FiredIntent;
pub use trade_control_core::trade_plan::{BarEvent, CrossDir, LinePoint};

mod cross;
mod driver;
mod effect;
mod facts;
mod plan;
mod rule;
mod world;

mod rules;

pub use driver::drive;
pub use effect::Effect;
pub use facts::{FactValue, Facts};
pub use plan::{Line, PlanRule, RuleKind, TradePlan};
pub use rule::Rule;
pub use rules::BreakAndClose;
pub use world::World;
