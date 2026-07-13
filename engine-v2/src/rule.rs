//! The [`Rule`] trait — the single seam the driver ticks.
//!
//! A rule reads the [`World`] (facts + candle + plan), may mutate the fact
//! blackboard (`World::state`), and returns the [`Effect`]s it wants done. The
//! driver interprets those effects (this slice: folds `Fire` into the
//! `PlanEval`). One rule per concept; the driver owns ordering and folding.

use trade_control_core::broker::Candle;
use trade_control_core::plan_eval::FiredIntent;
use trade_control_core::trade_plan::ConditionRule;

use crate::effect::Effect;
use crate::world::World;

/// A single trade-plan rule the driver ticks once per candle.
///
/// Slice-1 contract: `tick` reads `w`, may mutate `w.state` facts (the
/// break-and-close rule stamps `break_close_at`, latches its fire, advances the
/// phase), and returns the effects to fold. A rule that doesn't fire this
/// candle returns an empty `Vec`.
pub trait Rule {
    /// The rule's identity — its `rule_id` (alert basename, e.g.
    /// `03-prep-break-and-close`). Used for logging/attribution and to key the
    /// `fired` / `last_close` blackboards.
    fn rule_id(&self) -> &str;

    /// Tick this rule against one candle. Reads [`World`], may mutate
    /// `w.state`, and returns the [`Effect`]s produced this candle.
    fn tick(&self, w: &mut World) -> Vec<Effect>;
}

/// Build a [`FiredIntent`] for a rule that fired on `candle`, cloning the intent
/// verbatim with no pattern signal. Port of the old engine's `push_fire`
/// (`push_fire_signal` with `signal: None`) — preps / guards / M/W heartbeats
/// carry no candle-pattern geometry, and break-and-close is a prep.
pub(crate) fn fired_intent(rule: &ConditionRule, candle: &Candle) -> FiredIntent {
    FiredIntent {
        rule_id: rule.rule_id.clone(),
        intent: rule.intent.clone(),
        candle: *candle,
        signal: None,
    }
}
