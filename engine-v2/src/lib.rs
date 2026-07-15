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
//! - a v2 [`World`] (no `PlanState`, no phase — holds a read-only `&Facts` + the
//!   v2 plan),
//! - the [`Rule`] behaviour trait (pure: `tick(&World) -> Vec<Effect>`) and the
//!   [`Effect`] enum (`Fire` + `WriteFact` + `WriteScratch`),
//! - [`BreakAndClose`], the first rule, reusing the proven `cross.rs`
//!   line-projection,
//! - [`Retest`], the first fact **consumer** — it gates on the
//!   `("neckline", "break_close")` fact break-and-close produced, then writes
//!   `("neckline", "retest")` on a genuine (tolerance-decayed) retest cross, and
//! - a minimal [`tick_once`] entry point that ticks the plan's rules for **one**
//!   bar (the caller owns the bar loop), dispatching each by
//!   [`RuleKind`], and collects effects.
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
mod late_entry;
mod rule;
mod world;

mod rules;

// The v2 plan model + the `FactKind`/`LineName` marker traits live in the shared
// `trade-control-types-v2` crate (a plan builder depends on them without pulling
// in the engine). Re-exported here so an engine consumer keeps a single import
// surface and needn't depend on the types crate directly.
pub use trade_control_types_v2::{
    BreakClose, EntryMechanism, EntryOutcome, Expiry, FactKind, LastClose, Line, LineName,
    Neckline, NewsWindow, PlanRule, PrepMap, PriceLevel, Retest as RetestFact, RuleKind,
    TimeMarker, TooHigh, TooLow, TradePlan,
};

pub use driver::tick_once;
pub use effect::Effect;
pub use facts::{FactValue, Facts};
pub use late_entry::{LateEntry, LateEntryOrder, resolve as resolve_late_entry};
pub use rule::Rule;
// The expiry *rule* is re-exported as `ExpiryRule` — the bare `Expiry` name is the
// `TimeMarker` marker (re-exported above), mirroring how the fact `Retest` is
// `RetestFact` while the rule keeps the bare `Retest`.
pub use rules::{
    BreakAndClose, Enter, Expiry as ExpiryRule, Invalidate, Pause, Retest, is_break_and_close,
    is_enter, is_expiry, is_invalidate, is_pause, is_retest,
};
pub use world::World;
