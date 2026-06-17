//! The output types of the pure engine evaluator.
//!
//! These live in `core` (not the `engine` crate) so they can derive `serde`
//! without pulling serde into the pure-FSM `engine` crate, and so the
//! [`tick_bundle`](crate::tick_bundle) recorder can name them. The engine's
//! `evaluate_plan` returns a [`PlanEval`] carrying the [`FiredIntent`]s it
//! decided to dispatch; `engine::evaluate` re-exports both so its public
//! signature is unchanged.
//!
//! Neither derives `PartialEq`: [`Intent`] (carried on [`FiredIntent`]) doesn't,
//! and threading it through that whole graph isn't worth it. The replay diff and
//! the round-trip test compare the **serialized JSON** instead — which is the
//! right equality for float-bearing data anyway.

use serde::{Deserialize, Serialize};

use crate::broker::Candle;
use crate::intent::Intent;
use crate::plan_state::PlanState;
use crate::signals::LatchedSignal;

/// One intent the evaluator decided to fire this run, tagged with the candle
/// that triggered it. The wrapper synthesises a `Shell` from `candle` and
/// dispatches `intent` through the same handlers the webhook uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FiredIntent {
    /// The `rule_id` that fired — for logging / attribution.
    pub rule_id: String,
    /// The exact intent the TV alert would have POSTed, cloned from the rule.
    pub intent: Intent,
    /// The candle on which the trigger fired (its close/high/low/open/time
    /// become the dispatched `Shell`).
    pub candle: Candle,
    /// The latched candle-pattern signal, set only when a `PinePattern` (H&S
    /// enter) rule fired. The wrapper folds its geometry
    /// (`signal_high`/`signal_low`/`golden`/`signal_confirmed`/`recent_*`/…)
    /// onto the dispatched `Shell` so the H&S enter resolves its entry/SL/TP
    /// against the *pattern* extremes — exactly as the TV alert's `{{plot(...)}}`
    /// substitutions did. `None` for every non-Pine trigger (M/W, vetos, preps),
    /// which carry no pattern geometry.
    pub signal: Option<LatchedSignal>,
}

/// The result of one `evaluate_plan` run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEval {
    /// Intents to dispatch, in candle order.
    pub fired: Vec<FiredIntent>,
    /// The advanced state to persist. The watermark has moved to the last
    /// candle processed (or is unchanged when `new_candles` is empty).
    pub new_state: PlanState,
    /// True once the plan has reached [`Phase::Done`](crate::plan_state::Phase)
    /// — the wrapper clears the plan + state. (M/W plans never reach this via
    /// the spine; they end by TTL or a veto.)
    pub done: bool,
}
