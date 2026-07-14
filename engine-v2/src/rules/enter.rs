//! [`Enter`] — the entry rule, **fact-based & pure**. The first rule to emit an
//! **acquisitive** effect ([`Effect::PlaceOrder`]).
//!
//! The enter is the consumer that ties the prep chain together. It reads its
//! own [`PlanRule::preps`] map — the layered `line -> [ordered milestone kinds]`
//! precondition model (see the `engine_v2_enter_preps_layered` memory and
//! `SCOPING-rule-based-engine.md`) — and does nothing until **every** line's
//! chain is satisfied. Then it emits one [`Effect::PlaceOrder`] carrying the
//! fired intent and the entry [`EntryMechanism`](crate::plan::EntryMechanism).
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

use crate::effect::Effect;
use crate::plan::PlanRule;
use crate::rule::Rule;
use crate::world::World;

/// Shared-fact kind of the enter's **terminal entry outcome**, stamped by the
/// DRIVER (not this rule) when it resolves a placement — a real placement *or* a
/// missed catch-up. The enter reads it as its fire-once guard (single-shot: if
/// set, the enter is done). Keyed by the
/// enter's **rule id** (not a geometry line) so it is unique to this enter.
pub const KIND_ENTRY_OUTCOME: &str = "entry_outcome";

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
        if w.facts.is_set(&self.rule.id, KIND_ENTRY_OUTCOME) {
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
        // The milestone must be present AND a timestamp fact.
        let Some(at) = w.facts.at(line, kind) else {
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
