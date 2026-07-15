//! [`Pause`] — the economic-news entry-pause rule, **fact-based & pure**.
//!
//! The first **non-terminal, toggling** rule in engine-v2. Each tick it recomputes
//! whether the current instant is inside any of the plan's
//! [`pause_windows`](crate::TradePlan::pause_windows) — the `[event − before,
//! event]` standoffs around qualifying economic-news events — and maintains the
//! plan-scoped [`Paused`](crate::facts::Paused) flag. The
//! [`Enter`](super::Enter) reads that flag as a guard and does not place while it
//! is set; the flag clears at the window's end, so the trade resumes
//! automatically (v1's "entries resume at the event time").
//!
//! Like every rule it only *reads* the [`Facts`](crate::facts::Facts) blackboard
//! and mutates nothing — the flag write leaves as an [`Effect::WriteFact`].
//!
//! # Wall-clock `now`, not `candle.time`
//!
//! Membership is tested against the tick's **`now`** (wall-clock), matching v1's
//! PR2 gating for control-window edges: a 14:30 event's pause opens at 14:30, not
//! when the enclosing H1 bar closes. This is the *opposite* clock from the
//! [`Expiry`](super::Expiry) rule (which uses `candle.time`, the spine
//! `trade-expiry` semantics) — deliberately: pause is a *control* window, expiry
//! is a *spine* retirement. See [`eval_time`](crate::cross::eval_time)'s docs for
//! the same distinction on the v1 side.
//!
//! # Edge-triggered — only writes on a change
//!
//! The rule emits a [`WriteFact`](Effect::WriteFact) **only when the paused state
//! differs from the flag already on the blackboard** — set `Flag(true)` on
//! entering a window, `Flag(false)` on leaving, and *nothing* on a bar where the
//! state is unchanged. This keeps the effect stream quiet (no redundant write
//! every bar) and is why [`flag_named`](crate::facts::Facts::flag_named)
//! distinguishes `Flag(false)` from unset: an unset flag on the first
//! outside-any-window bar must still be treated as "already not paused" so the
//! rule doesn't emit a spurious `Flag(false)`.
//!
//! # NOT fire-once — it must keep toggling
//!
//! Unlike the prep/retire rules there is no fire-once guard: a plan can enter and
//! leave several pause windows over its life (multiple news events), so the rule
//! stays live for every tick.

use crate::PlanRule;
use crate::effect::Effect;
use crate::facts::{FactKind, PLAN_SCOPE};
use crate::facts::{FactValue, Paused};
use crate::rule::Rule;
use crate::world::World;

/// The economic-news entry-pause rule, bound to a v2 [`PlanRule`]. No geometry
/// type parameter — it reads the plan-level
/// [`pause_windows`](crate::TradePlan::pause_windows), not a line/level/marker.
/// Borrowed so instantiating it per tick is free.
pub struct Pause<'r> {
    /// The rule this wraps.
    pub rule: &'r PlanRule,
}

impl<'r> Pause<'r> {
    /// Wrap a pause [`PlanRule`].
    pub fn new(rule: &'r PlanRule) -> Self {
        Self { rule }
    }
}

impl Rule for Pause<'_> {
    fn rule_id(&self) -> &str {
        &self.rule.id
    }

    fn tick(&self, w: &World) -> Vec<Effect> {
        // Are we inside any pause window right now? Tested against wall-clock `now`
        // (see module docs) — the window boundaries are real event minutes.
        let paused_now = w.plan.pause_windows.iter().any(|win| win.contains(w.now));

        // The flag currently on the blackboard. `None` (never written) is treated
        // as "not paused" so the first outside-window bar doesn't emit a spurious
        // clear.
        let flag_now = w
            .facts
            .flag_named(PLAN_SCOPE, Paused::NAME)
            .unwrap_or(false);

        // Edge-triggered: only write when the state actually changes.
        if paused_now == flag_now {
            return Vec::new();
        }

        vec![Effect::WriteFact {
            line: PLAN_SCOPE.to_string(),
            kind: Paused::NAME.to_string(),
            value: FactValue::Flag(paused_now),
        }]
    }
}
