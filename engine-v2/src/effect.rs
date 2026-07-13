//! [`Effect`] — what a rule *emits*: a thing it wants the driver to do.
//!
//! An effect is something *to do*, deliberately distinct from an *event*
//! (something that happened) — keeping the two words separate avoids the
//! confusion that dogged early design discussion (see
//! `SCOPING-rule-based-engine.md`).
//!
//! # Slice 1 surface
//!
//! Only [`Effect::Fire`] exists this slice — the driver folds it into the
//! returned [`PlanEval::fired`] list, exactly as the old engine's `push_fire`
//! appended to its `fired` vec. Fact writes (`break_close_at`, the fire latch,
//! the phase transition) are **not** effects yet: for this slice a rule mutates
//! `World::state` directly, matching the old engine's in-place mutation of
//! `PlanState`. A later slice can formalise facts-as-effects (`WriteFact`) and
//! add `PlaceOrder` / `CancelPending` / … once the `Broker` trait lands.

use trade_control_core::plan_eval::FiredIntent;

/// What a [`Rule`](crate::rule::Rule) asks the driver to do after a tick.
///
/// Minimal for slice 1: dispatching a fired intent is the only externally
/// visible action break-and-close produces. The variant carries a fully-formed
/// [`FiredIntent`] so the driver just collects it — the rule owns the shape of
/// what it fires (rule_id, cloned intent, triggering candle), exactly as the
/// old engine's `push_fire` did.
#[derive(Debug, Clone)]
pub enum Effect {
    /// Dispatch this intent — the driver appends it to [`PlanEval::fired`].
    ///
    /// [`PlanEval`]: trade_control_core::plan_eval::PlanEval
    Fire(FiredIntent),
}
