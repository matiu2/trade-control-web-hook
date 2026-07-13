//! The [`Rule`] trait — the single seam the driver ticks (behaviour), plus the
//! [`fired_intent`] helper that turns a fired [`PlanRule`] into a [`FiredIntent`].
//!
//! A rule is a **pure** function of the [`World`] (facts + candle + plan): it
//! reads facts other rules wrote (and its own scratch), and returns **all** its
//! outputs as [`Effect`]s — fires *and* fact/scratch writes. It never mutates the
//! world. The driver interprets those effects: applies the writes to the shared
//! [`Facts`](crate::facts::Facts) and collects the `Fire`s.
//!
//! Note the name split (see [`crate::plan`]): the **data** struct is
//! [`PlanRule`], this **behaviour** trait is `Rule`.

use trade_control_core::broker::Candle;
use trade_control_core::plan_eval::FiredIntent;

use crate::effect::Effect;
use crate::plan::PlanRule;
use crate::world::World;

/// A single rule the driver ticks once per candle.
///
/// A rule that doesn't act this candle returns an empty `Vec`.
pub trait Rule {
    /// The rule's identity — its [`PlanRule::id`]. Used for attribution and to
    /// key facts.
    fn rule_id(&self) -> &str;

    /// Tick this rule against one candle. **Pure**: reads [`World`] and returns
    /// the [`Effect`]s produced this candle (fires + fact/scratch writes). Must
    /// not mutate anything — the driver applies the returned writes.
    fn tick(&self, w: &World) -> Vec<Effect>;
}

/// Build a [`FiredIntent`] for a rule that fired on `candle`, cloning the intent
/// verbatim with no pattern signal (preps carry no candle-pattern geometry, and
/// break-and-close is a prep).
pub(crate) fn fired_intent(rule: &PlanRule, candle: &Candle) -> FiredIntent {
    FiredIntent {
        rule_id: rule.id.clone(),
        intent: rule.intent.clone(),
        candle: *candle,
        signal: None,
    }
}
