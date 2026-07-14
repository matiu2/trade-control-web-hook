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
//! - [`Effect::PlaceOrder`] — the first **acquisitive** effect: the enter rule
//!   emits it once its preps are satisfied. Unlike the writes above it is
//!   **live-bar-only** — the driver's `apply` drops it on a stale backlog bar
//!   (real-money catch-up safety). Executing it (the async `Broker` call) is a
//!   separate driver step, not part of the pure tick.
//!
//! A later slice adds `CancelPending` / `WidenStop` / … as the spread systems
//! land; those too are effects the driver executes.
//!
//! [`facts`]: crate::facts

use trade_control_core::plan_eval::FiredIntent;

use crate::facts::FactValue;
use crate::plan::EntryMechanism;

/// What a [`Rule`](crate::rule::Rule) asks the driver to do after a tick.
///
/// [`Fire`](Effect::Fire) boxes its [`FiredIntent`] — that payload is large
/// (~1.3 KB) and would otherwise bloat every `Effect` (including the small write
/// variants) to its size. Boxing keeps `Effect` cheap to move around in the
/// per-tick `Vec`.
#[derive(Debug, Clone)]
pub enum Effect {
    /// Dispatch this intent — the driver appends it to the fired list it returns
    /// to the caller of [`tick_once`](crate::driver::tick_once).
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
    /// Place an entry order — the first **acquisitive** effect (the enter rule
    /// emits it once its preps are all satisfied).
    ///
    /// # Acquisitive ⇒ latest-bar-only (real-money catch-up safety)
    ///
    /// Unlike the timeless fact/scratch writes, a `PlaceOrder` **must never be
    /// executed on a stale backlog bar** — placing an order now for a signal
    /// hours ago chases a dead price. The gate lives in the driver's `apply`
    /// (keyed on `tick_once`'s `latest_bar`), NOT in the rule: the rule stays pure
    /// and mode-blind and always emits this; the **driver drops it on a stale
    /// backlog bar** and keeps it only on the latest bar. See
    /// `SCOPING-rule-based-engine.md`, "Catch-up policy after downtime".
    ///
    /// The [`FiredIntent`] payload is boxed for the same size reason as
    /// [`Fire`](Effect::Fire). [`mechanism`](Effect::PlaceOrder::mechanism) tells
    /// the (later, async) executor how to place — stop/limit/market; this slice
    /// executes only [`EntryMechanism::Stop`].
    ///
    /// # `trigger_price` + `candle_close` — so a late resolve needs no re-derivation
    ///
    /// The effect carries the two numbers [`late_entry::resolve`] needs to decide
    /// missed-vs-place-late on a caught-up bar, stamped at fire time so the
    /// resolver never has to re-derive them from the intent:
    ///
    /// - `trigger_price` — the price the order is trying to place *at*
    ///   (`None` for a market order, which has no resting trigger).
    /// - `candle_close` — the close of the bar the enter fired on, the reference
    ///   "price when we decided" that the parity check compares against.
    ///
    /// The same two are what the live-system spread-hour restore case will need
    /// (when resting orders are pulled before the 07:00-Brisbane spread hour and
    /// restored after — price may have moved, so restoring is the same
    /// missed-vs-place-late question). See `[[spread_hour_rubbish_candle_suppression]]`.
    ///
    /// [`late_entry::resolve`]: crate::late_entry::resolve
    PlaceOrder {
        /// The fired enter intent (rule id + intent + the firing candle).
        fired: Box<FiredIntent>,
        /// How to place the order (stop/limit/market).
        mechanism: EntryMechanism,
        /// The price the order is trying to place *at* — the resting trigger.
        /// `None` for a market order (no resting trigger) and, in this slice,
        /// wherever the trigger has not yet been resolved from the intent's
        /// `EntrySpec` (that resolution is the executor's job, a later slice); the
        /// enter emits `None` today and the executor fills it in.
        trigger_price: Option<f64>,
        /// The close of the bar the enter fired on — the reference price the
        /// late-resolve parity check compares against. Filled by the enter from
        /// its firing candle.
        candle_close: f64,
    },
}
