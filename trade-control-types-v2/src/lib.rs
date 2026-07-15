//! `trade-control-types-v2` — the **shared v2 plan model**.
//!
//! This crate holds the *data* every v2 participant agrees on: the serializable
//! trade-plan shape a builder writes and the engine consumes, plus the
//! compile-time name types (`FactKind`, `LineName` + their markers) that key the
//! fact blackboard. It deliberately holds **no behaviour** — no `Rule`, no
//! `Effect`, no `Facts`, no driver, no cross maths. Those are the engine's
//! private machinery ([`trade_control_engine_v2`]); a *builder* only needs to
//! **describe** a plan, never tick one.
//!
//! # Who shares this
//!
//! ```text
//!            trade-control-types-v2   (this crate)
//!            ╱          │           ╲
//!     engine-v2    tv-arm-v2    trade-control-v2 (cli)
//! ```
//!
//! - **engine-v2** consumes a plan (ticks its rules) and re-exports these types
//!   for its own users, so an engine consumer needn't depend on this crate
//!   directly.
//! - **tv-arm-v2** / **trade-control-v2** (not yet built) *construct* a plan — a
//!   chart-armer and a CLI spec-builder respectively. They depend on this crate
//!   for the plan shape and the `LineName`/`FactKind` markers to name preps.
//!
//! Splitting the model out here is what stops a builder from having to pull in
//! the whole engine (cross maths, late-entry resolver, the rule impls) just to
//! emit a `TradePlan`.
//!
//! # Wire format
//!
//! `TradePlan` and its parts are `serde`-serializable; the fact key names
//! ([`FactKind::NAME`], [`LineName::NAME`]) are the stable strings the store keys
//! on. A rename of any `NAME` is a persisted-state migration — the
//! `*_names_are_stable` tests pin them.

mod fact_kind;
mod line_name;
mod plan;
mod price_level;
mod time_marker;
mod window;

pub use fact_kind::{
    BreakClose, EntryOutcome, FactKind, Invalidated, LastClose, PLAN_SCOPE, Retest,
};
pub use line_name::{Expiry, LineName, Neckline, TooHigh, TooLow};
pub use plan::{EntryMechanism, Line, PlanRule, PrepMap, RuleKind, TradePlan};
pub use price_level::PriceLevel;
pub use time_marker::TimeMarker;
pub use window::NewsWindow;
