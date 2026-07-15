//! [`FactKind`] — a fact's *kind* as a compile-time type, not a string.
//!
//! A fact is keyed by `(line, kind)` (shared) or `(rule_id, kind)` (scratch). The
//! **kind** half is always known to the rule at compile time — the retest rule
//! *always* writes `Retest`, break-and-close *always* writes `BreakClose` — so it
//! is a natural fit for a type rather than a stringly-typed literal sprinkled
//! across the codebase. Modelling it as a type makes name collisions a **compile
//! error** and lets a setup crate (H&S, M/W, future trend-following) define its
//! own kinds without a central enum everyone must edit.
//!
//! # Zero-size markers + a trait (open set, per-crate)
//!
//! Each kind is a zero-size marker struct implementing [`FactKind`], whose only
//! payload is a stable [`FactKind::NAME`]. That name is what actually lands in the
//! serialized `Facts` blackboard (the engine's, in `trade-control-engine-v2`) —
//! the on-the-wire format is unchanged, still strings — so the type layer is a
//! **compile-time convenience over a string-keyed store**, not a new wire format.
//! Rules refer to kinds by type (`facts.set::<BreakClose, Neckline>(…)`); the
//! store holds `NAME`.
//!
//! An open trait (rather than one closed `enum FactKind`) is deliberate: kinds
//! live in whatever crate owns the rule that writes them. The four below are the
//! ones the current slice needs; a new setup adds its own marker structs in its
//! own crate.
//!
//! The **line** half of the key is the sibling [`LineName`](crate::LineName)
//! trait (also here); `rule_id` (scratch) is inherently runtime and stays a
//! string.

/// A fact kind, identified by a stable serialized [`NAME`](FactKind::NAME).
///
/// Implemented by zero-size marker structs (below). The `NAME` is the string the
/// engine's `Facts` store keys on and serializes — keep it stable across releases
/// (it is persisted state).
pub trait FactKind {
    /// The stable, serialized name of this kind (e.g. `"break_close"`). Persisted
    /// — do not rename without a migration.
    const NAME: &'static str;
}

/// `break_close` — a line's break-and-close stamp (shared fact). Written by the
/// break-and-close rule, read by retest (as its producer gate) and the enter.
pub struct BreakClose;
impl FactKind for BreakClose {
    const NAME: &'static str = "break_close";
}

/// `retest` — a line's retest stamp (shared fact). Written by the retest rule,
/// read by the enter.
pub struct Retest;
impl FactKind for Retest {
    const NAME: &'static str = "retest";
}

/// `entry_outcome` — an enter's terminal outcome (shared fact, keyed by the
/// enter's rule id). Stamped by the DRIVER (placed or missed); read by the enter
/// as its fire-once guard.
pub struct EntryOutcome;
impl FactKind for EntryOutcome {
    const NAME: &'static str = "entry_outcome";
}

/// `last_close` — a rule's prior-close bookkeeping (**rule-private scratch**, keyed
/// by rule id). The prior close an `OnClose` cross measures against.
pub struct LastClose;
impl FactKind for LastClose {
    const NAME: &'static str = "last_close";
}

/// `invalidated` — the plan's **terminal retire** stamp (shared fact, keyed by
/// the plan-scope sentinel [`PLAN_SCOPE`], not a line or a rule id). Written by
/// the DRIVER when it applies an
/// [`Effect::Invalidate`](../../trade_control_engine_v2/enum.Effect.html): an
/// invalidation cap (`too_high`/`too_low`) has been crossed, so the setup's
/// thesis is dead. Read by the enter as a **second fire-once guard** — a
/// retired plan never enters. `StopNextEntry`-only: it blocks entry, it does not
/// close a position (v2 is single-shot with no open position to manage).
pub struct Invalidated;
impl FactKind for Invalidated {
    const NAME: &'static str = "invalidated";
}

/// The reserved "line" slot for **plan-scoped** shared facts — facts about the
/// whole plan rather than a single line. Chosen with surrounding double
/// underscores so it can never collide with a real [`LineName`](crate::LineName)
/// (`"neckline"`/`"too_high"`/…) or an operator-assigned rule id. Used today by
/// the [`Invalidated`] retire fact.
pub const PLAN_SCOPE: &str = "__plan__";
