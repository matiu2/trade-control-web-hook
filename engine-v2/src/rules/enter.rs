//! [`Enter`] — the entry rule, **fact-based & pure**. The first rule to emit an
//! **acquisitive** effect ([`Effect::PlaceOrder`]).
//!
//! The enter is the consumer that ties the prep chain together. It reads its
//! own [`PlanRule::preps`] map — the layered `line -> [ordered milestone kinds]`
//! precondition model (see the `engine_v2_enter_preps_layered` memory and
//! `SCOPING-rule-based-engine.md`) — and does nothing until **every** line's
//! chain is satisfied. Then it emits one [`Effect::PlaceOrder`] carrying the
//! fired intent and the entry [`EntryMechanism`](crate::EntryMechanism).
//!
//! Like the preps it only *reads* the [`Facts`](crate::facts::Facts) blackboard
//! and mutates nothing; the `PlaceOrder` leaves as an effect for the driver to
//! (later, asynchronously) execute against the `Broker`.
//!
//! # The satisfaction check (pure)
//!
//! For each `(line, kinds)` entry in [`PlanRule::preps`]:
//!
//! 1. **All milestones present** — every kind in `kinds` has a fact set at
//!    `(line, kind)`, stored as an `At(time)`.
//! 2. **Ordered within the line** — the fact-times are **strictly increasing**
//!    in `kinds` list order: `t(kinds[0]) < t(kinds[1]) < …`. This re-asserts the
//!    order the operator declared (`break_close` *then* `retest`); the retest's
//!    own producer-gate already enforces it, but the enter is the single place
//!    the sequence is *declared*, so it checks it too.
//!
//! Lines are **independent** — no ordering constraint *between* different lines;
//! each line's chain is checked on its own and **all** lines must pass. `preps`
//! is a map, not a global sequence.
//!
//! An empty `preps` map ⇒ a **no-prep enter** (vacuously satisfied) — it places
//! as soon as it ticks on the live bar. That is a legitimate trade shape (a
//! market-structure entry with no break/retest), expressed as data, not a
//! distinct rule.
//!
//! # Fire-once — off a DRIVER-stamped terminal outcome (not a rule write)
//!
//! Without a fire-once guard the enter would emit `PlaceOrder` on *every* bar its
//! preps hold — double-placing. But the enter must **not** stamp its own guard:
//! on a backlog bar the driver may resolve the placement to *missed* or
//! *place-late* (see below), and a rule-written guard would be set on a bar whose
//! placement was then dropped, silently losing a still-valid setup (the catch-up
//! trap).
//!
//! So the guard is a **fact the DRIVER stamps** when it resolves an acquisitive
//! effect to a terminal outcome — `(rule_id, "entry_outcome")`, set to the bar
//! time on a real placement (latest bar, or a caught-up place-late) **and** on a
//! *missed* (the counterfactual trade already played out — don't re-enter later).
//! The enter only **reads** it: if it's set, the enter is **done** and emits
//! nothing. Keyed by the enter's **rule id** (not a geometry line) so multiple
//! enters in one plan finish independently, and it can never collide with a line
//! fact. (Multi-shot re-entry — firing repeatedly *without* going done — is a
//! later rule; see `[[multishot_engine_keeps_plan_alive]]`. This slice is
//! single-shot.)
//!
//! # Second fire-once — plan retirement (invalidation)
//!
//! The enter also reads a **plan-scoped** retire fact `(PLAN_SCOPE,
//! "invalidated")`, stamped by the driver when it applies an
//! [`Effect::Invalidate`](crate::effect::Effect::Invalidate) from an
//! [`Invalidate`](crate::rules::Invalidate) rule: an invalidation cap
//! (`too_high`/`too_low`) was crossed, so the setup is dead. If it's set the enter
//! is done — same "read a driver-stamped fact, emit nothing" shape as
//! `entry_outcome`, but plan-scoped (one invalidation retires the whole plan)
//! rather than rule-id-scoped. It is `StopNextEntry`-only: it blocks entry, it
//! never closes a position (single-shot, no position management yet).
//!
//! # NO catch-up / late-entry logic here (it's the driver's job)
//!
//! The enter emits `PlaceOrder` on **every** bar its preconditions hold — it does
//! NOT check whether this is the latest bar, nor simulate the gap. That
//! real-money catch-up safety ("place now iff it's parity with never having been
//! down") is resolved by the driver via [`late_entry`](crate::late_entry), keyed
//! on `tick_once`'s `latest_bar`. Keeping it out of the rule is deliberate: the
//! rule stays pure and mode-blind, so replay and live exercise identical rule
//! logic and differ only in the `Broker`/`Storage` impls.

use trade_control_core::plan_eval::FiredIntent;

use crate::PlanRule;
use crate::effect::Effect;
use crate::facts::{EntryOutcome, FactKind, Invalidated, PLAN_SCOPE};
use crate::rule::Rule;
use crate::world::World;

/// The entry rule, bound to a v2 [`PlanRule`]. Borrowed so instantiating it per
/// tick is free.
pub struct Enter<'r> {
    /// The rule this wraps.
    pub rule: &'r PlanRule,
}

impl<'r> Enter<'r> {
    /// Wrap an enter [`PlanRule`].
    pub fn new(rule: &'r PlanRule) -> Self {
        Self { rule }
    }
}

impl Rule for Enter<'_> {
    fn rule_id(&self) -> &str {
        &self.rule.id
    }

    fn tick(&self, w: &World) -> Vec<Effect> {
        // Fire-once: once the driver has stamped a terminal entry outcome for this
        // enter (placed OR missed), it is done (single-shot). The rule only READS
        // this fact — the driver writes it when it resolves the placement, so a
        // backlog bar whose placement the driver later drops never marks this enter
        // done.
        // Keyed by the enter's **rule id** in the line slot (a runtime string, not
        // a geometry `LineName`) — so this uses the by-name accessor. See the
        // `entry_outcome` note above and `facts` on the two namespaces.
        if w.facts.is_set_named(&self.rule.id, EntryOutcome::NAME) {
            return Vec::new();
        }

        // Second fire-once guard: if the plan has been RETIRED — an invalidation
        // cap (`too_high`/`too_low`) was crossed and the driver stamped
        // `(PLAN_SCOPE, "invalidated")` — the setup's thesis is dead. The enter is
        // done and never places, even if its preps would otherwise be satisfied.
        // Plan-scoped (not rule-id-scoped like `entry_outcome`) because one
        // invalidation retires the whole plan. StopNextEntry-only: this blocks the
        // entry; nothing here closes a position (v2 is single-shot).
        if w.facts.is_set_named(PLAN_SCOPE, Invalidated::NAME) {
            return Vec::new();
        }

        // The current closed bar (last of the window). Absent only for an empty
        // window, which the driver already guards.
        let Some(candle) = w.current() else {
            return Vec::new();
        };

        // All prep chains must be satisfied — all milestones present AND strictly
        // ordered within each line, lines independent.
        if !self.preps_satisfied(w) {
            return Vec::new();
        }

        // Emit the placement, unconditionally. Whether it becomes a real order
        // now, a caught-up place-late, or a logged "missed" is the DRIVER's call
        // (via `late_entry`), keyed on `latest_bar`. The rule stays pure and
        // mode-blind — it does not know or care whether this is the latest bar.
        let fired = FiredIntent {
            rule_id: self.rule.id.clone(),
            intent: self.rule.intent.clone(),
            candle: *candle,
            signal: None,
        };
        vec![Effect::PlaceOrder {
            fired: Box::new(fired),
            mechanism: self.rule.mechanism,
            // The resting trigger is resolved from the intent's `EntrySpec` by the
            // executor (a later slice); the enter doesn't compute it, so `None` for
            // now. `candle_close` IS known here — the close of the firing bar, the
            // reference the late-resolve parity check compares against.
            trigger_price: None,
            candle_close: candle.c,
        }]
    }
}

impl Enter<'_> {
    /// Is every prep line satisfied? For each `(line, kinds)`: all `kinds` present
    /// as `At(time)` facts AND their times strictly increasing in list order.
    /// Lines are independent; **all** must pass. An empty map is vacuously true
    /// (no-prep enter).
    fn preps_satisfied(&self, w: &World) -> bool {
        self.rule
            .preps
            .iter()
            .all(|(line, kinds)| line_chain_satisfied(w, line, kinds))
    }
}

/// One line's milestone chain: every `kind` in `kinds` has an `At(time)` fact at
/// `(line, kind)`, and those times are **strictly increasing** in `kinds` order.
///
/// A non-`At` fact value (or a missing one) fails the whole chain. An empty
/// `kinds` list is vacuously satisfied (a declared-but-empty line — unusual, but
/// not an error).
fn line_chain_satisfied(w: &World, line: &str, kinds: &[String]) -> bool {
    let mut prev: Option<chrono::DateTime<chrono::Utc>> = None;
    for kind in kinds {
        // The milestone must be present AND a timestamp fact. This is the one read
        // where the kind is genuinely RUNTIME — it comes from the enter's `preps`
        // map (plan data listing milestone kind *names*), so it uses the by-name
        // accessor, not the typed `at::<K>`.
        let Some(at) = w.facts.at_named(line, kind) else {
            return false;
        };
        // Strictly after the previous milestone in this line's chain.
        if let Some(prev_at) = prev
            && at <= prev_at
        {
            return false;
        }
        prev = Some(at);
    }
    true
}
