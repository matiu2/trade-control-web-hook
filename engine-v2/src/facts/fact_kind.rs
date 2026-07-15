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
//! serialized [`Facts`](super::Facts) blackboard (the on-the-wire format is
//! unchanged — still strings), so the type layer is a **compile-time convenience
//! over a string-keyed store**, not a new wire format. Rules refer to kinds by
//! type (`facts.set::<Neckline, BreakClose>(…)`); the store holds `NAME`.
//!
//! An open trait (rather than one closed `enum FactKind`) is deliberate: kinds
//! live in whatever crate owns the rule that writes them. The four below are the
//! ones the current slice needs; a new setup adds its own marker structs in its
//! own crate.
//!
//! The **line** half of the key is *not* here — it stays a runtime string until
//! the typed-geometry slice (see `SCOPING-engine-v2-typed-geometry.md`, 4b), and
//! `rule_id` (scratch) is inherently runtime. Only the kind is typed in this
//! slice.

/// A fact kind, identified by a stable serialized [`NAME`](FactKind::NAME).
///
/// Implemented by zero-size marker structs (below). The `NAME` is the string the
/// [`Facts`](super::Facts) store keys on and serializes — keep it stable across
/// releases (it is persisted state).
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
