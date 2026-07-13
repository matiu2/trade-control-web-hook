//! The driver loop — ticks the plan's rules per candle and collects their
//! effects.
//!
//! v2 shape: **no phase, no ordering intelligence, no seeding of a state
//! machine.** For each candle the driver builds a fresh [`World`] over the
//! shared [`Facts`] blackboard and ticks each break-and-close rule in plan
//! order, collecting every [`Effect`] returned. Rules coordinate purely through
//! the facts they write.
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

/// Drive `plan` over `candles`, ticking its break-and-close rules and mutating
/// the shared `facts` blackboard in place. Returns every [`Effect`] the rules
/// emitted across all candles, in tick order.
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
    let mut effects = Vec::new();

    for candle in candles {
        for rule in plan.rules.iter().filter(|r| is_break_and_close(r)) {
            let bc = BreakAndClose::new(rule);
            let mut world = World {
                now,
                candle: Some(candle),
                window,
                facts,
                plan,
            };
            effects.extend(bc.tick(&mut world));
        }
    }

    effects
}
