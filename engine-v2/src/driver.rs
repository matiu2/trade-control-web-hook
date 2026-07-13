//! The driver loop — ticks the plan's (pure) rules per candle, **applies** their
//! write effects to the shared blackboard, and collects their fires.
//!
//! v2 shape: **no phase, no ordering intelligence, no seeding of a state
//! machine.** Rules are pure ([`Rule::tick`] takes `&World`); the driver is the
//! single site that mutates [`Facts`]. For each candle it builds a fresh
//! read-only [`World`] over the shared blackboard, ticks each break-and-close
//! rule in plan order, and applies every [`Effect`] returned:
//! [`WriteFact`](Effect::WriteFact) / [`WriteScratch`](Effect::WriteScratch) are
//! written into `facts`; [`Fire`](Effect::Fire) is collected for the caller.
//!
//! # Effect-application ordering (LOAD-BEARING)
//!
//! A rule *reads* facts a **prior** candle's effects wrote (its `break_close`
//! latch, its `last_close` scratch). So the driver must apply a candle's write
//! effects to `facts` **before** moving to the next candle — otherwise the next
//! candle's tick reads stale state and, e.g., an `OnClose` cross measures against
//! the wrong prior close. We apply each rule's effects immediately after it
//! ticks (before the next rule and before the next candle). With one rule this is
//! just "tick → apply → next candle"; when multiple rules interact on the *same*
//! candle, applying each rule's writes before the next rule ticks means a later
//! rule in list order already sees an earlier rule's writes this candle — the
//! producer/consumer chain (break-and-close → retest → enter) is correct by the
//! baked list order (see `SCOPING-rule-based-engine.md`, "Ordering — baked by
//! tv-arm").
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

/// Drive `plan` over `candles`, ticking its (pure) break-and-close rules,
/// applying their write effects to the shared `facts` blackboard in place, and
/// returning the [`Fire`](Effect::Fire) effects across all candles in tick order.
///
/// The returned vec is Fires only — the caller-facing contract is "what fired".
/// Fact/scratch writes are *applied* to `facts` here (that's the driver's job),
/// not handed back. See the module docs for the load-bearing "apply before the
/// next candle" ordering.
///
/// - `facts` — the fact blackboard, carried across ticks and mutated in place
///   (the persisted state in later slices).
/// - `candles` — the ascending candles to process this drive.
/// - `window` — the ascending detector window a sloped line resolves its level
///   against (bar-index space). Typically the same series as (or a superset of)
///   `candles`.
/// - `now` — the tick's wall-clock instant.
pub fn drive(
    plan: &TradePlan,
    facts: &mut Facts,
    candles: &[Candle],
    window: &[Candle],
    now: DateTime<Utc>,
) -> Vec<Effect> {
    let mut fires = Vec::new();

    for candle in candles {
        for rule in plan.rules.iter().filter(|r| is_break_and_close(r)) {
            let bc = BreakAndClose::new(rule);
            // A fresh read-only World over the current facts. Scoped so the
            // shared `&facts` borrow ends before we apply the effects below.
            let effects = {
                let world = World {
                    now,
                    candle: Some(candle),
                    window,
                    facts,
                    plan,
                };
                bc.tick(&world)
            };
            // Apply this rule's writes to `facts` immediately (before the next
            // rule and the next candle), and collect its fires.
            apply(facts, &mut fires, effects);
        }
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
