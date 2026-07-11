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

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::broker::Candle;
use crate::intent::Intent;
use crate::plan_state::PlanState;
use crate::signals::LatchedSignal;

/// Decline reason emitted when a `needs_golden` enter sees a non-golden signal.
/// Shared so the engine's write side ([`crate::plan_eval::EntryDecline`]) and
/// the replay's presentation filter agree on the exact wording — the replay
/// suppresses this reason when the operator asked to mark golden-only candles
/// (a "not golden" decline is tautological noise in that view).
pub const NOT_GOLDEN_DECLINE: &str = "needs golden but signal is not golden";

/// Decline reason emitted when a `needs_confirmed` enter sees an unconfirmed
/// signal. Shared for the same reason as [`NOT_GOLDEN_DECLINE`].
pub const NOT_CONFIRMED_DECLINE: &str = "needs confirmation but signal is not confirmed";

/// A bar on which a `PinePattern` enter's signal fired + matched direction, but
/// the enter was **declined** before it could fire an intent — the pure,
/// recomputable-here pre-flight rejections (`pine_entry_dispatchable`): the
/// candle-quality gate (`needs_golden` / `needs_confirmed`) or a bracket
/// `resolve` failure (below-min-R, out-of-range, degenerate geometry, …).
///
/// This is the "the golden printed but nothing entered — why?" surface. The
/// engine used to swallow the reason at `tracing::debug!`; recording it here
/// lets the offline replay show it right on the bar, no `RUST_LOG` needed.
///
/// Diagnostic only — like [`PlanEval::warnings`], it recomputes from the same
/// inputs and is **not** part of the replay diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryDecline {
    /// Open-time of the bar the enter declined on.
    pub bar: DateTime<Utc>,
    /// The enter rule that declined (e.g. `05-enter`).
    pub rule_id: String,
    /// Human-readable reason, already worded for display (e.g.
    /// `needs golden but signal is not golden`, `needs confirmation but signal
    /// is not confirmed`, `trade R=0.914 is below the required minimum of
    /// 1.000`).
    pub reason: String,
}

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
    /// Non-fatal diagnostics from this run that the pure evaluator can't log
    /// itself (the `engine` crate has no worker logging). Today this carries
    /// **trendline out-of-window anchor** warnings: a `TrendlineCross` whose
    /// anchor predates / postdates the fetched candle window can only have its
    /// bar-index *estimated* by the signed `bar_seconds` divisor (reintroducing
    /// wall-clock spacing across any gap in the un-fetched span), and with
    /// `bar_seconds == 0` (pre-field plans) the trendline silently can't fire at
    /// all. The wrapper (`run_engine_tick`) `rlog!`s these so the degraded path
    /// is visible in Cloudflare logs instead of being a silent extrapolation.
    /// `#[serde(default)]` keeps tick bundles recorded before this field
    /// deserialisable; it is **not** part of the replay diff (the diff compares
    /// `fired` / `new_state` / `done` only — warnings recompute from the same
    /// recorded inputs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Per-bar `PinePattern` enter declines: a signal fired + matched the plan
    /// direction, but the pre-flight (`pine_entry_dispatchable`) rejected it
    /// (needs-golden / needs-confirmed / resolve-failed), so no intent fired and
    /// the plan stayed armed. The offline replay renders these on the bar so
    /// "golden seen but no entry" reads its own reason. `#[serde(default)]` keeps
    /// older tick bundles deserialisable; **not** part of the replay diff (it
    /// recomputes from the same inputs, exactly like `warnings`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_declines: Vec<EntryDecline>,
}

impl PlanEval {
    /// Is this tick worth recording a full [`TickBundle`](crate::tick_bundle)
    /// for?
    ///
    /// A tick is **noteworthy** if it fired anything, finished the plan, or
    /// advanced the FSM's *meaningful* state versus `prior`. A **no-op** tick —
    /// a new closed bar arrived, but nothing fired, the plan isn't done, and the
    /// FSM didn't actually move — carries no information worth a fat bundle (the
    /// bundle re-stores the whole plan, both states, and the wide detector
    /// window), so the wrapper trims it to a heartbeat log instead.
    ///
    /// **The watermark/TTL gotcha.** `new_state` is *not* compared as a whole:
    /// every tick advances `watermark` to the new candle's time, refreshes
    /// `expires_at` to a fresh TTL stamp, and (for an `OnClose` cross rule)
    /// records `last_close` — so a full-struct `!=` would be true on *every*
    /// tick and nothing would ever be a no-op. We compare only the
    /// FSM-meaningful fields (phase, fire latches, break-and-close / retest
    /// stamps, the reserved `mw` slot) via [`PlanState::advanced_vs`], which
    /// ignores `watermark`, `expires_at`, and `last_close` (see that method for
    /// why `last_close` is bookkeeping, not a meaningful advance).
    pub fn is_noteworthy(&self, prior: &PlanState) -> bool {
        !self.fired.is_empty() || self.done || self.new_state.advanced_vs(prior)
    }
}
