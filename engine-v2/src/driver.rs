//! The driver loop — ticks the plan's rules per candle and folds their effects
//! into a [`PlanEval`].
//!
//! # Slice 1 scope
//!
//! This slice interprets **only** the break-and-close spine phase. Per candle,
//! while the plan is in [`Phase::AwaitBreakAndClose`], the driver builds a
//! [`World`] and ticks the plan's break-and-close rule; a `Fire` effect is
//! folded into `PlanEval::fired`, and the rule's own mutation of `World::state`
//! carries the `break_close_at` stamp, the fire latch, and the phase advance.
//!
//! Controls, guards, retest, and entry are **not** interpreted here (later
//! slices). The parity harness therefore exercises break-and-close-only plans,
//! where the old engine's controls/guards/entry arms produce nothing and the
//! driver's output is byte-identical on the parity-critical fields
//! (`break_close_at`, the break-and-close `fired` entry, `done`).
//!
//! [`seed_plan_state`] and [`initial_phase`] mirror the old engine's functions
//! of the same name so a plan seeds identically before the first tick.

use chrono::{DateTime, Utc};

use trade_control_core::broker::Candle;
use trade_control_core::plan_eval::PlanEval;
use trade_control_core::plan_state::{Phase, PlanState};
use trade_control_core::trade_plan::TradePlan;

use crate::effect::Effect;
use crate::rule::Rule;
use crate::rules::{BreakAndClose, is_break_and_close};
use crate::world::World;

/// The starting spine phase for a plan. Port of the old engine's
/// [`initial_phase`] restricted to slice-1's world: a plan carrying a
/// break-and-close prep starts gated behind it; anything else starts watching
/// for entry directly.
///
/// The old engine's multi-enter (strategy-v2) exception — which starts a
/// two-enter plan in `AwaitEntry` even with a break-and-close rule present — is
/// out of scope for slice 1 (no entry rules are interpreted). The parity harness
/// uses single-enter break-and-close plans, for which this matches the old
/// engine exactly.
pub fn initial_phase(plan: &TradePlan) -> Phase {
    if plan.rules.iter().any(is_break_and_close) {
        Phase::AwaitBreakAndClose
    } else {
        Phase::AwaitEntry
    }
}

/// Seed a fresh plan's state from a back-window of recent candles **without
/// firing anything**. Port of the old engine's `seed_plan_state`.
///
/// Sets the watermark to the newest candle's open-time and records each
/// break-and-close rule's `last_close` from that candle (only `OnClose`
/// triggers track it), so the *next* tick can detect a cross against it.
/// `fired` stays empty; the phase is [`initial_phase`].
///
/// Slice-1 note: the old engine seeds `last_close` for *every* rule; here only
/// the break-and-close rule exists in the interpreted set, and only its
/// `OnClose` variant records anything — byte-identical for the `last_close` key
/// the break-and-close cross reads next tick.
pub fn seed_plan_state(
    plan: &TradePlan,
    candles: &[Candle],
    expires_at: DateTime<Utc>,
) -> PlanState {
    let mut state = PlanState::seed(initial_phase(plan), expires_at);
    let Some(newest) = candles.iter().max_by_key(|c| c.time) else {
        return state;
    };
    state.watermark = Some(newest.time);
    for rule in plan.rules.iter().filter(|r| is_break_and_close(r)) {
        if crate::cross::trigger_uses_close(&rule.trigger) {
            state.last_close.insert(rule.rule_id.clone(), newest.c);
        }
    }
    state
}

/// Drive a plan through the candles that have closed since its watermark,
/// interpreting the break-and-close spine (slice 1).
///
/// Same signature shape as the old engine's `evaluate_plan`: `prior` is the
/// persisted state, `new_candles` the ascending candles `> prior.watermark`,
/// `detector_window` the back-window a `TrendlineCross` resolves its level
/// against, `now` the tick time, `expires_at` the TTL stamp.
///
/// Returns a [`PlanEval`] whose `fired` holds the break-and-close prep intent(s)
/// that fired, `new_state` the advanced state (with `break_close_at` stamped and
/// the phase advanced on fire), and `done` (always `false` this slice — nothing
/// interpreted here retires a plan; entry/guards do, and they're later slices).
pub fn drive(
    plan: &TradePlan,
    prior: &PlanState,
    new_candles: &[Candle],
    detector_window: &[Candle],
    now: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> PlanEval {
    let mut state = prior.clone();
    state.expires_at = expires_at;
    let mut fired = Vec::new();

    for candle in new_candles {
        // Sequential spine — slice 1 interprets only the break-and-close phase.
        if state.phase == Phase::AwaitBreakAndClose {
            tick_break_and_close(plan, &mut state, candle, detector_window, now, &mut fired);
        }
        state.watermark = Some(candle.time);
    }

    let done = state.phase == Phase::Done;
    PlanEval {
        fired,
        new_state: state,
        done,
        warnings: Vec::new(),
        entry_declines: Vec::new(),
    }
}

/// Tick the plan's break-and-close rule for one candle, folding a `Fire` effect
/// into `fired`. Mirrors the old engine's `evaluate_break_and_close`, including
/// its "no break-and-close rule but we're in its phase → advance immediately"
/// defensive arm (here: no rule to tick, so advance the phase directly).
fn tick_break_and_close(
    plan: &TradePlan,
    state: &mut PlanState,
    candle: &Candle,
    window: &[Candle],
    now: DateTime<Utc>,
    fired: &mut Vec<trade_control_core::plan_eval::FiredIntent>,
) {
    let Some(rule) = plan.rules.iter().find(|r| is_break_and_close(r)) else {
        // Defensive: in the break-and-close phase with no break-and-close rule —
        // advance immediately (matches the old engine).
        state.phase = Phase::AwaitEntry;
        return;
    };

    let bc = BreakAndClose::new(rule);
    let mut world = World {
        now,
        candle: Some(candle),
        window,
        state,
        plan,
    };
    for effect in bc.tick(&mut world) {
        match effect {
            Effect::Fire(fi) => fired.push(fi),
        }
    }
}
