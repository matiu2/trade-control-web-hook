//! The driver — ticks the plan's (pure) rules for **one** bar, **applies** their
//! write effects to the shared blackboard, and collects their fires.
//!
//! v2 shape: **no phase, no ordering intelligence, no seeding of a state
//! machine.** Rules are pure ([`Rule::tick`] takes `&World`); the driver is the
//! single site that mutates [`Facts`]. For the current bar it builds a fresh
//! read-only [`World`] over the shared blackboard, ticks **every** rule in plan
//! order — dispatching each [`PlanRule`](crate::PlanRule) to the matching
//! [`Rule`] impl by its [`RuleKind`](crate::RuleKind) — and applies every
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
//! [`RuleKind`](crate::RuleKind). The enter emits the first **acquisitive**
//! effect ([`Effect::PlaceOrder`]); the driver gates it on `latest_bar` (see
//! [`apply`]) but does **not** yet *execute* it — running the async `Broker` call
//! is a separate driver step added with the executor. So `tick_once` stays pure
//! and sync; `Broker`/`Storage` thread into that later execute step, not here.

use chrono::{DateTime, Utc};

use trade_control_core::broker::Candle;

use crate::effect::Effect;
use crate::facts::{FactKind, FactValue, Facts, Invalidated, PLAN_SCOPE};
use crate::rule::Rule;
use crate::rules::{BreakAndClose, Enter, Expiry, Invalidate, Retest};
use crate::world::World;
// The `Expiry` LineName marker and the `Expiry` rule share a name — alias the
// marker so `Expiry::<ExpiryMarker>` in `tick_rule` reads unambiguously.
use crate::{Expiry as ExpiryMarker, Neckline, PlanRule, RuleKind, TooHigh, TooLow, TradePlan};

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
        // `apply`): on a stale backlog bar a `PlaceOrder` is dropped. `now` stamps
        // the plan-scoped retire fact for an `Invalidate`.
        apply(facts, &mut fires, effects, latest_bar, now);
    }

    fires
}

/// Tick one [`PlanRule`] via the [`Rule`] impl matching its
/// [`RuleKind`](crate::RuleKind). The impls borrow the rule, so this
/// constructs the impl inline and ticks it (no per-rule boxing needed).
///
/// The geometry-generic rules ([`BreakAndClose`], [`Retest`], [`Invalidate`],
/// [`Expiry`](crate::rules::Expiry)) are generic over the
/// [`LineName`](crate::LineName) they target; the driver binds that name from the
/// `kind`. The preps target [`Neckline`](crate::Neckline); the invalidation caps
/// target [`TooHigh`](crate::TooHigh) / [`TooLow`](crate::TooLow) (horizontal
/// [`PriceLevel`](crate::PriceLevel)s); the expiry targets
/// [`Expiry`](crate::Expiry) (a [`TimeMarker`](crate::TimeMarker)) — one kind per
/// target so the geometry is fixed by the type, never a runtime string.
fn tick_rule(rule: &PlanRule, world: &World) -> Vec<Effect> {
    match rule.kind {
        RuleKind::BreakAndClose => BreakAndClose::<Neckline>::new(rule).tick(world),
        RuleKind::Retest => Retest::<Neckline>::new(rule).tick(world),
        RuleKind::Enter => Enter::new(rule).tick(world),
        // The invalidation caps bind their level from the kind, same as the preps
        // bind `Neckline` — `TooHigh`/`TooLow` are `PriceLevel`s, crossed with no
        // projection inside the rule.
        RuleKind::InvalidateHigh => Invalidate::<TooHigh>::new(rule).tick(world),
        RuleKind::InvalidateLow => Invalidate::<TooLow>::new(rule).tick(world),
        // Trade-expiry binds the `Expiry` `TimeMarker` — a wall-clock cutoff,
        // crossed by `candle.time >= marker` (no price) inside the rule.
        RuleKind::Expiry => Expiry::<ExpiryMarker>::new(rule).tick(world),
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
fn apply(
    facts: &mut Facts,
    fires: &mut Vec<Effect>,
    effects: Vec<Effect>,
    latest_bar: bool,
    now: DateTime<Utc>,
) {
    for effect in effects {
        match effect {
            // The effect carries line/kind as strings, and the driver applies
            // them via the by-name setters — NOT because the typed `Facts` API is
            // avoided here, but because the driver is downstream of a deliberate
            // type-erasure boundary: a heterogeneous `Vec<Effect>` (writes from
            // several rules, each with its own K/L) can't stay generic, so each
            // rule erased its `K`/`L` into `K::NAME`/`L::NAME` when it built the
            // effect. The strings are always resolved NAMEs, never hand-typed
            // literals. Full reasoning on `Effect`'s docs ("Why the write variants
            // key on `String`"). Don't try to route this back through `set::<K, L>`.
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
            // Terminal retire: stamp the plan-scoped `invalidated` fact so the
            // (pure) enter observes the retirement on the blackboard — its second
            // fire-once guard — then fold the effect into `fires` so the caller
            // sees the terminal signal explicitly. Timeless like a fact write
            // (NOT acquisitive): it applies on a backlog bar too, so a cap that
            // broke during downtime still retires the plan on catch-up.
            Effect::Invalidate { .. } => {
                facts.set_named(PLAN_SCOPE, Invalidated::NAME, FactValue::At(now));
                fires.push(effect);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Applying an [`Effect::Invalidate`] stamps the plan-scoped `invalidated`
    /// retire fact **and** folds the effect into the returned list — the two halves
    /// of the terminal-retire wiring. Exercises the private `apply` directly (the
    /// full rule → cross → retire path is the step-4 integration test).
    #[test]
    fn invalidate_stamps_retire_fact_and_is_returned() {
        let mut facts = Facts::default();
        let mut fires = Vec::new();
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();

        assert!(
            !facts.is_set_named(PLAN_SCOPE, Invalidated::NAME),
            "not retired before the invalidate",
        );

        apply(
            &mut facts,
            &mut fires,
            vec![Effect::Invalidate {
                rule_id: "01-veto-too-high".into(),
            }],
            true,
            now,
        );

        assert!(
            facts.is_set_named(PLAN_SCOPE, Invalidated::NAME),
            "the retire fact is stamped so the enter's guard sees it",
        );
        assert_eq!(
            facts.at_named(PLAN_SCOPE, Invalidated::NAME),
            Some(now),
            "stamped at the tick's `now`",
        );
        assert!(
            matches!(fires.as_slice(), [Effect::Invalidate { .. }]),
            "the Invalidate effect is returned to the caller as the terminal signal",
        );
    }

    /// The retire is **not** gated by `latest_bar` — a cap that broke on a stale
    /// backlog bar still retires the plan (invalidation is timeless knowledge,
    /// like a fact write, unlike an acquisitive `PlaceOrder`).
    #[test]
    fn invalidate_applies_on_a_backlog_bar() {
        let mut facts = Facts::default();
        let mut fires = Vec::new();
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();

        apply(
            &mut facts,
            &mut fires,
            vec![Effect::Invalidate {
                rule_id: "01-veto-too-high".into(),
            }],
            false, // NOT the latest bar
            now,
        );

        assert!(
            facts.is_set_named(PLAN_SCOPE, Invalidated::NAME),
            "invalidation applies on a backlog bar, unlike an acquisitive PlaceOrder",
        );
    }
}
