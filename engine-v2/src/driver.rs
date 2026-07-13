//! The driver — ticks the plan's (pure) rules for **one** bar, **applies** their
//! write effects to the shared blackboard, and collects their fires.
//!
//! v2 shape: **no phase, no ordering intelligence, no seeding of a state
//! machine.** Rules are pure ([`Rule::tick`] takes `&World`); the driver is the
//! single site that mutates [`Facts`]. For the current bar it builds a fresh
//! read-only [`World`] over the shared blackboard, ticks each break-and-close
//! rule in plan order, and applies every [`Effect`] returned:
//! [`WriteFact`](Effect::WriteFact) / [`WriteScratch`](Effect::WriteScratch) are
//! written into `facts`; [`Fire`](Effect::Fire) is collected for the caller.
//!
//! # One bar per call — the caller owns the loop
//!
//! [`tick_once`] processes **exactly one bar** (the current bar is
//! `window.last()`). There is no internal candle loop: the **caller** drives the
//! bar stream, calling `tick_once` once per bar in ascending order and passing
//! that bar's growing window each time. `facts` is threaded across those calls
//! (the caller owns it), so a fact one bar's tick wrote is visible to the next.
//!
//! # Effect-application ordering (LOAD-BEARING)
//!
//! A rule *reads* facts a **prior** bar's effects wrote (its `break_close`
//! latch, its `last_close` scratch). Two orderings keep this correct:
//!
//! - **Across bars (the caller's job):** the caller must apply this bar's write
//!   effects — which `tick_once` does in place, into the `facts` it was handed —
//!   *before* it calls `tick_once` for the next bar. Because `facts` is mutated
//!   in place and carried across calls, driving the bars in order is sufficient:
//!   the next bar's tick reads the state this bar left. Skip or reorder bars and
//!   an `OnClose` cross measures against the wrong prior close.
//! - **Within a bar (the driver's job):** when multiple rules interact on the
//!   *same* bar, `tick_once` applies each rule's writes before the next rule
//!   ticks, so a later rule in list order already sees an earlier rule's writes
//!   this bar — the producer/consumer chain (break-and-close → retest → enter)
//!   is correct by the baked list order (see `SCOPING-rule-based-engine.md`,
//!   "Ordering — baked by tv-arm").
//!
//! Slice 1 instantiates only break-and-close rules (`kind ==
//! RuleKind::BreakAndClose`). No `Broker`/`Storage` yet — those thread in when
//! the entry rules land.

use chrono::{DateTime, Utc};

use trade_control_core::broker::Candle;

use crate::effect::Effect;
use crate::facts::Facts;
use crate::plan::TradePlan;
use crate::rule::Rule;
use crate::rules::{BreakAndClose, is_break_and_close};
use crate::world::World;

/// Tick `plan`'s (pure) break-and-close rules for **one** bar — the current bar
/// is `window.last()` — applying their write effects to the shared `facts`
/// blackboard in place and returning the [`Fire`](Effect::Fire) effects this bar
/// produced, in tick order.
///
/// The caller owns the bar loop: call this once per bar in ascending order,
/// passing that bar's ascending window (the detector series *ending at* the bar
/// under evaluation). `facts` carries across calls, so this bar's writes are
/// visible to the next — see the module docs for the load-bearing "apply before
/// the next bar" ordering (which, because `facts` is mutated in place here, the
/// caller satisfies simply by driving bars in order).
///
/// The returned vec is Fires only — the caller-facing contract is "what fired".
/// Fact/scratch writes are *applied* to `facts` here (that's the driver's job),
/// not handed back.
///
/// - `plan` — the v2 plan whose rules are ticked.
/// - `facts` — the fact blackboard, carried across ticks and mutated in place
///   (the persisted state in later slices).
/// - `window` — the ascending detector series **ending at the current bar**
///   (`window.last()` is the bar being processed); a sloped line resolves its
///   level in bar-index space against this same series. Empty ⇒ no bar to
///   process, returns `Vec::new()`.
/// - `now` — the tick's wall-clock instant. A rule derives bar-relative
///   properties (staleness, mid-bar) from `now` vs the current bar's time; the
///   driver stamps no "is-live"/"is-replay" flag (that would smuggle
///   mode-branching into the rules — replay and live must differ only in the
///   `Broker`/`Storage` impls).
pub fn tick_once(
    plan: &TradePlan,
    facts: &mut Facts,
    window: &[Candle],
    now: DateTime<Utc>,
) -> Vec<Effect> {
    // No bar ⇒ nothing to do. Rules read the current bar via `World::current`
    // (`window.last()`); the guard here just avoids ticking on an empty window.
    if window.is_empty() {
        return Vec::new();
    }

    let mut fires = Vec::new();

    for rule in plan.rules.iter().filter(|r| is_break_and_close(r)) {
        let bc = BreakAndClose::new(rule);
        // A fresh read-only World over the current facts. Scoped so the shared
        // `&facts` borrow ends before we apply the effects below.
        let effects = {
            let world = World {
                now,
                window,
                facts,
                plan,
            };
            bc.tick(&world)
        };
        // Apply this rule's writes to `facts` immediately (before the next rule),
        // and collect its fires.
        apply(facts, &mut fires, effects);
    }

    fires
}

/// Apply one tick's `effects`: write facts/scratch into `facts`, collect fires
/// into `fires`. This is the ONLY place `facts` is mutated.
fn apply(facts: &mut Facts, fires: &mut Vec<Effect>, effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::WriteFact { line, kind, value } => facts.set(&line, &kind, value),
            Effect::WriteScratch {
                rule_id,
                kind,
                value,
            } => facts.set_scratch(&rule_id, &kind, value),
            Effect::Fire(_) => fires.push(effect),
        }
    }
}
