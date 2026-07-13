//! [`Effect`] — what a rule *emits*: a thing it wants the driver to do.
//!
//! An effect is something *to do*, deliberately distinct from an *event*
//! (something that happened) — keeping the two words separate avoids the
//! confusion that dogged early design discussion (see
//! `SCOPING-rule-based-engine.md`).
//!
//! # Pure rules — ALL output is an effect
//!
//! Rules are pure: `tick(&World) -> Vec<Effect>` (see [`Rule`](crate::rule::Rule)).
//! A rule never mutates the world; every output — the intent it fires **and**
//! every fact/scratch write — leaves the rule as an [`Effect`]. The **driver** is
//! the single site that mutates [`Facts`](crate::facts::Facts) and (later) does
//! broker I/O, by applying these effects in order.
//!
//! # Slice 1 surface
//!
//! - [`Effect::Fire`] — dispatch a fired intent; the driver folds it into the
//!   returned fired list, exactly as the old engine's `push_fire` appended to its
//!   `fired` vec.
//! - [`Effect::WriteFact`] — record a **shared** trade fact keyed `(line, kind)`
//!   (e.g. `("neckline", "break_close")`). Other rules read these as real trade
//!   state.
//! - [`Effect::WriteScratch`] — record a **rule-private** scratch value keyed
//!   `(rule_id, kind)` (e.g. break-and-close's `last_close` cross bookkeeping).
//!   Scratch lives in its own namespace so a future rule can never read another
//!   rule's private bookkeeping as if it were a trade fact (see [`facts`]).
//!
//! Two write variants (rather than one with a shared-vs-scratch flag) because the
//! two namespaces are keyed differently — shared by `(line, kind)`, scratch by
//! `(rule_id, kind)` — so a single variant would carry an unused key half in each
//! mode. The split keeps each variant's fields exactly the key it needs.
//!
//! A later slice adds `PlaceOrder` / `CancelPending` / `WidenStop` / … once the
//! `Broker` trait lands; those too are effects the driver executes.
//!
//! [`facts`]: crate::facts

use trade_control_core::plan_eval::FiredIntent;

use crate::facts::FactValue;

/// What a [`Rule`](crate::rule::Rule) asks the driver to do after a tick.
///
/// [`Fire`](Effect::Fire) boxes its [`FiredIntent`] — that payload is large
/// (~1.3 KB) and would otherwise bloat every `Effect` (including the small write
/// variants) to its size. Boxing keeps `Effect` cheap to move around in the
/// per-tick `Vec`.
#[derive(Debug, Clone)]
pub enum Effect {
    /// Dispatch this intent — the driver appends it to the fired list it returns
    /// to the caller of [`drive`](crate::driver::drive).
    Fire(Box<FiredIntent>),
    /// Record a **shared** trade fact at `(line, kind)`. The driver applies it to
    /// the plan's [`Facts`](crate::facts::Facts) so the next candle's ticks see it.
    WriteFact {
        /// The line the fact is about (e.g. `"neckline"`).
        line: String,
        /// The fact kind (e.g. `"break_close"`).
        kind: String,
        /// The value to store.
        value: FactValue,
    },
    /// Record a **rule-private** scratch value at `(rule_id, kind)`. Kept in a
    /// separate namespace from shared facts (see [`Effect`] docs).
    WriteScratch {
        /// The owning rule's id (e.g. `"03-prep-break-and-close"`).
        rule_id: String,
        /// The scratch kind (e.g. `"last_close"`).
        kind: String,
        /// The value to store.
        value: FactValue,
    },
}
