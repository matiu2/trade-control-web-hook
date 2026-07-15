//! The driver — ticks the plan's (pure) rules for **one** bar, **applies** their
//! write effects to the shared blackboard, and collects their fires.
//!
//! v2 shape: **no phase, no ordering intelligence, no seeding of a state
//! machine.** Rules are pure ([`Rule::tick`] takes `&World`); the driver is the
//! single site that mutates [`Facts`]. For the current bar it builds a fresh
//! read-only [`World`] over the shared blackboard, ticks **every** rule in plan
//! order — dispatching each [`PlanRule`](crate::plan::PlanRule) to the matching
//! [`Rule`] impl by its [`RuleKind`](crate::plan::RuleKind) — and applies every
//! [`Effect`] returned: [`WriteFact`](Effect::WriteFact) /
//! [`WriteScratch`](Effect::WriteScratch) are written into `facts`;
//! [`Fire`](Effect::Fire) is collected for the caller.
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
//! fire-once fact, its `last_close` scratch). Two orderings keep this correct:
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
//!   "Ordering — baked by tv-arm"). This is load-bearing: a retest rule listed
//!   *after* its break-and-close in `plan.rules` sees the `break_close` fact
//!   that break-and-close wrote **earlier this same bar**.
//!
//! Rules instantiated: break-and-close, retest, and enter, dispatched by
//! [`RuleKind`](crate::plan::RuleKind). The enter emits the first **acquisitive**
//! effect ([`Effect::PlaceOrder`]); the driver gates it on `latest_bar` (see
//! [`apply`]) but does **not** yet *execute* it — running the async `Broker` call
//! is a separate driver step added with the executor. So `tick_once` stays pure
//! and sync; `Broker`/`Storage` thread into that later execute step, not here.

use chrono::{DateTime, Utc};

use trade_control_core::broker::Candle;

use crate::effect::Effect;
use crate::facts::Facts;
use crate::plan::{Neckline, PlanRule, RuleKind, TradePlan};
use crate::rule::Rule;
use crate::rules::{BreakAndClose, Enter, Retest};
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
/// - `latest_bar` — is the current bar the **newest available** to the caller?
///   `true` for every bar in normal ticking and replay; `false` only for the
///   older bars of a post-downtime catch-up backlog. It names a property of the
///   bar ("this is the freshest one I have"), **not** a live-vs-replay mode — the
///   distinction the whole engine is built to avoid. It gates only acquisitive
///   effects in [`apply`] (a stale backlog bar must not place an order for a
///   signal hours ago); fact/scratch writes ignore it.
pub fn tick_once(
    plan: &TradePlan,
    facts: &mut Facts,
    window: &[Candle],
    now: DateTime<Utc>,
    latest_bar: bool,
) -> Vec<Effect> {
    // No bar ⇒ nothing to do. Rules read the current bar via `World::current`
    // (`window.last()`); the guard here just avoids ticking on an empty window.
    if window.is_empty() {
        return Vec::new();
    }

    let mut fires = Vec::new();

    // Tick EVERY rule in plan order, dispatching each to its behaviour impl by
    // `kind`. Effects are applied to `facts` immediately (before the next rule)
    // so a later rule this same bar sees an earlier rule's writes — the
    // producer/consumer chain (break-and-close → retest) is correct by list
    // order. See the module docs' "within a bar" ordering note.
    for rule in plan.rules.iter() {
        // A fresh read-only World over the current facts. Scoped so the shared
        // `&facts` borrow ends before we apply the effects below.
        let effects = {
            let world = World {
                now,
                window,
                facts,
                plan,
            };
            tick_rule(rule, &world)
        };
        // Apply this rule's writes to `facts` immediately (before the next rule),
        // and collect its fires. `latest_bar` gates acquisitive effects (see
        // `apply`): on a stale backlog bar a `PlaceOrder` is dropped.
        apply(facts, &mut fires, effects, latest_bar);
    }

    fires
}

/// Tick one [`PlanRule`] via the [`Rule`] impl matching its
/// [`RuleKind`](crate::plan::RuleKind). The impls borrow the rule, so this
/// constructs the impl inline and ticks it (no per-rule boxing needed).
///
/// The producer rules ([`BreakAndClose`], [`Retest`]) are generic over the line
/// [`LineName`](crate::plan::LineName) they target; the driver binds that line
/// from the `kind`. In the current setup vocabulary both target
/// [`Neckline`](crate::plan::Neckline) — when an invalidation rule targeting
/// `TooHigh`/`TooLow` lands, it gets its own `RuleKind` arm binding that line.
fn tick_rule(rule: &PlanRule, world: &World) -> Vec<Effect> {
    match rule.kind {
        RuleKind::BreakAndClose => BreakAndClose::<Neckline>::new(rule).tick(world),
        RuleKind::Retest => Retest::<Neckline>::new(rule).tick(world),
        RuleKind::Enter => Enter::new(rule).tick(world),
    }
}

/// Apply one tick's `effects`: write facts/scratch into `facts`, collect fires
/// (and latest-bar `PlaceOrder`s) into `fires`. This is the ONLY place `facts` is
/// mutated.
///
/// # Catch-up gate (real-money safety)
///
/// `latest_bar` is `true` only for the **newest** bar the caller is processing
/// (normally every tick; `false` only for the older bars of a post-downtime
/// catch-up backlog). Effects are gated by category — the fact-based model maps
/// cleanly onto "what is safe to do on a stale bar" (see
/// `SCOPING-rule-based-engine.md`, "Catch-up policy after downtime"):
///
/// - **Fact/scratch writes** are *timeless knowledge* → always applied, on a
///   backlog bar or the latest one. "The neckline broke at bar -5" is true
///   whenever we learn it, so the `break_close`/`retest` facts catch up across
///   the backlog.
/// - **`PlaceOrder`** is *acquisitive* → **latest bar only.** Dropping it on a
///   stale backlog bar is the whole point: never place an order for a signal that
///   fired hours ago. Because the facts above caught up, when the enter re-ticks
///   on the *latest* bar its preconditions are already satisfied — so it enters at
///   the current price iff the setup is still valid now, and doesn't otherwise.
///
/// (`Fire` — the prep dispatch — is neither: it is folded into `fires`
/// regardless. No prep is acquisitive; only `PlaceOrder` is gated.)
fn apply(facts: &mut Facts, fires: &mut Vec<Effect>, effects: Vec<Effect>, latest_bar: bool) {
    for effect in effects {
        match effect {
            // The effect carries the kind as a runtime string (a rule already
            // resolved it from `K::NAME` when it built the effect), so the driver
            // applies it via the by-name setters.
            Effect::WriteFact { line, kind, value } => facts.set_named(&line, &kind, value),
            Effect::WriteScratch {
                rule_id,
                kind,
                value,
            } => facts.set_scratch_named(&rule_id, &kind, value),
            Effect::Fire(_) => fires.push(effect),
            // Acquisitive: keep only on the latest bar; drop on a stale backlog bar.
            Effect::PlaceOrder { .. } => {
                if latest_bar {
                    fires.push(effect);
                }
            }
        }
    }
}
