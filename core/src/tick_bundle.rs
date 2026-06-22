//! The replayable record of one engine **tick**, scoped to one plan.
//!
//! The cron engine's `tick_one` (in the worker crate) is a pure function of a
//! typed tuple — `(plan, prior_state, new_candles, detector_window, now,
//! expires_at)` in, [`PlanEval`] out. A [`TickBundle`] captures exactly that
//! tuple plus the golden output, so a tick can be replayed offline against the
//! same `evaluate_plan` and the result diffed. It is the engine-era analogue of
//! the webhook's [`RequestRecord`](crate::tick_bundle) (recorded per inbound
//! HTTP request); a tick records here instead, since the cron tick — not an HTTP
//! alert — is where every trading decision now happens.
//!
//! # Unit of recording: one `(tick, plan)`
//!
//! One bundle per plan evaluated, not one per cron run. `tick_one` already owns
//! exactly this tuple in one scope; `trade_id` is the aggregate key, so a single
//! trade's whole life replays by globbing its R2 prefix. The cron run fans out
//! to N independent plan chains, so the bundle's `request_id` is per-`(tick,
//! plan)`.
//!
//! # WASM-safe, serde round-trips
//!
//! Lives in `core` (already serde-aware, WASM-built); every field round-trips
//! (see the module test). The recorder I/O — writing this to R2 fire-and-forget
//! — lives in the worker glue, never here: this type is pure data.
//!
//! # Correlation keys
//!
//! The field names track the roadmap's event schema (`roadmap/src/
//! event-schema.md`): `correlation_id` = the plan's `trade_id`, `tick_ts` = the
//! cron `now` (the roadmap's `ts`), `request_id` = one `(tick, plan)` causal
//! chain, and per fired intent a [`DispatchOutcome`] carries its `intent_id`
//! (= the intent's `id`) and an intra-tick `seq`. A later projection can fan a
//! bundle out into the roadmap's per-event stream; the bundle is the raw
//! capture, not that stream.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::broker::Candle;
use crate::plan_eval::PlanEval;
use crate::plan_state::PlanState;
use crate::trade_plan::TradePlan;

/// The current [`TickBundle`] schema version. Bump on any breaking field change
/// so a replay can refuse a bundle it can't faithfully restore.
pub const TICK_BUNDLE_SCHEMA_VERSION: u32 = 1;

/// One recorded engine tick for one plan — the full replay tuple plus its golden
/// [`PlanEval`] output. Serialize this to R2 under the `ticks/` prefix; replay
/// it by re-running `evaluate_plan` on the inputs and diffing against `eval`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickBundle {
    /// Schema version; [`TICK_BUNDLE_SCHEMA_VERSION`] at write time.
    pub schema_version: u32,

    // --- correlation keys (roadmap event-schema.md) ---
    /// The cron tick instant (`now`). The roadmap's `ts`; cross-tick ordering.
    pub tick_ts: DateTime<Utc>,
    /// The plan's `trade_id` — the aggregate key (one setup's whole life).
    pub correlation_id: String,
    /// `None` for a globally-scoped plan; `Some(name)` for an account-scoped one.
    pub account: Option<String>,
    /// One `(tick, plan)` causal chain id. Minted from `(trade_id, tick_ts)` —
    /// the cron run fans out to N plan chains, so this is per-plan, not per-run.
    pub request_id: String,

    // --- inputs: the replay tuple (`evaluate_plan` arguments) ---
    /// The static registered plan as evaluated this tick.
    pub plan: TradePlan,
    /// The persisted [`PlanState`] read in at the start of the tick.
    pub prior_state: PlanState,
    /// The candles closed since the watermark that drove the FSM this tick.
    pub new_candles: Vec<Candle>,
    /// The wider detector back-window (H&S Pine lookback); equals `new_candles`
    /// for a plan with no `PinePattern` entry.
    pub detector_window: Vec<Candle>,
    /// The tick instant passed to `evaluate_plan` (`== tick_ts`; kept explicit
    /// because the FSM's expiry/watermark logic is a function of it).
    pub now: DateTime<Utc>,
    /// The TTL stamp passed to `evaluate_plan` (load-bearing for the FSM).
    pub expires_at: DateTime<Utc>,

    // --- golden output: assert a replay against this ---
    /// The [`PlanEval`] `evaluate_plan` returned (fired / new_state / done).
    pub eval: PlanEval,
    /// `plan.shadow` at record time — was dispatch observe-only this tick?
    pub shadow: bool,
    /// One per fired intent the tick dispatched. Empty for a shadow tick (which
    /// dispatches nothing) and for a tick that fired nothing.
    pub dispatch_outcomes: Vec<DispatchOutcome>,

    // --- KV transition for the plan-state row this tick ---
    /// The before/after of the plan-state KV write, with success/error.
    pub kv: KvTickTransition,
}

impl TickBundle {
    /// The R2 object key: `ticks/<date>/<tick_ts>-<trade_id>.json`. A new
    /// top-level prefix, sibling to the webhook's `req/` and the roadmap's
    /// `events/`, so the downstream `req/`-reader never trips on tick-bundles.
    pub fn r2_key(&self) -> String {
        let ts = self.tick_ts.to_rfc3339();
        let date = ts.get(..10).unwrap_or("unknown");
        let tid = &self.correlation_id;
        format!("ticks/{date}/{ts}-{tid}.json")
    }
}

/// The outcome of dispatching one fired intent, recorded for the golden replay.
/// The worker maps its `ActionResult` into the `outcome` string at the call site
/// (the `ActionResult` itself wraps a non-serializable `worker::Response`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchOutcome {
    /// The `rule_id` of the fired rule (= [`FiredIntent::rule_id`](crate::plan_eval::FiredIntent)).
    pub rule_id: String,
    /// The fired intent's `id` (the roadmap's `intent_id` — the fire key).
    pub intent_id: String,
    /// The dispatch result string (e.g. `Ok(entered)`, `rejected: veto-active`),
    /// from `ActionResult::describe()`.
    pub outcome: String,
    /// Intra-tick fire ordering (the roadmap's `seq`) — wall-clock can't order
    /// fires that share a tick instant.
    pub seq: u32,
}

/// The before/after of this tick's plan-state KV write, the roadmap's
/// `KvTransition` scoped to one tick's `plan-state:` row. `success`/`error` make
/// "the engine wanted to advance state but the write failed" a first-class,
/// queryable fact (the roadmap's highest-value debug record).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KvTickTransition {
    /// The KV key written: `plan-state:{scope}:{trade_id}`.
    pub key: String,
    /// The prior state loaded in (`None` on the tick right after a seed).
    pub before: Option<PlanState>,
    /// The state persisted out (`None` when the plan finished and state was
    /// cleared).
    pub after: Option<PlanState>,
    /// True when the plan reached `done` and the plan row itself was cleared too.
    pub cleared_plan: bool,
    /// Whether the KV write succeeded.
    pub success: bool,
    /// The write error string when `success == false`.
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-authored JSON bundle covering every field, including a fired
    /// intent (so the `Intent` graph deserializes) and a non-empty
    /// `new_candles`/`detector_window`. Built from JSON rather than a 45-field
    /// `Intent` literal so the fixture survives `Intent` gaining fields, and so
    /// the parse itself exercises the `Deserialize` path the bundle relies on.
    const SAMPLE: &str = r#"{
      "schema_version": 1,
      "tick_ts": "2026-06-17T20:00:00Z",
      "correlation_id": "hs-copper-c474f303",
      "account": "experimental",
      "request_id": "hs-copper-c474f303@2026-06-17T20:00:00Z",
      "plan": {
        "trade_id": "hs-copper-c474f303",
        "instrument": "Copper",
        "direction": "short",
        "granularity": "h1",
        "pip_size": 0.1,
        "rules": [
          {
            "rule_id": "05-enter",
            "trigger": { "type": "pine_pattern", "dir": "short" },
            "fire_mode": "once",
            "intent": {
              "v": 1,
              "id": "hs-copper-c474f303-enter",
              "not_after": "2026-06-21T21:16:50Z",
              "action": "enter",
              "instrument": "Copper",
              "direction": "short",
              "broker": "tradenation",
              "account": "experimental",
              "trade_id": "hs-copper-c474f303"
            }
          }
        ],
        "shadow": false
      },
      "prior_state": {
        "watermark": "2026-06-17T19:00:00Z",
        "phase": "await_entry",
        "fired": ["03-prep-break-and-close"],
        "last_close": {},
        "break_close_at": null,
        "retest_seen_at": null,
        "mw": null,
        "expires_at": "2026-06-18T20:00:00Z"
      },
      "new_candles": [
        { "time": "2026-06-17T19:00:00Z", "o": 13500.0, "h": 13520.0, "l": 13490.0, "c": 13510.0 }
      ],
      "detector_window": [
        { "time": "2026-06-17T18:00:00Z", "o": 13480.0, "h": 13505.0, "l": 13470.0, "c": 13500.0 },
        { "time": "2026-06-17T19:00:00Z", "o": 13500.0, "h": 13520.0, "l": 13490.0, "c": 13510.0 }
      ],
      "now": "2026-06-17T20:00:00Z",
      "expires_at": "2026-06-18T20:00:00Z",
      "eval": {
        "fired": [
          {
            "rule_id": "05-enter",
            "intent": {
              "v": 1,
              "id": "hs-copper-c474f303-enter",
              "not_after": "2026-06-21T21:16:50Z",
              "action": "enter",
              "instrument": "Copper",
              "direction": "short",
              "broker": "tradenation",
              "account": "experimental",
              "trade_id": "hs-copper-c474f303"
            },
            "candle": { "time": "2026-06-17T19:00:00Z", "o": 13500.0, "h": 13520.0, "l": 13490.0, "c": 13510.0 },
            "signal": null
          }
        ],
        "new_state": {
          "watermark": "2026-06-17T19:00:00Z",
          "phase": "done",
          "fired": ["03-prep-break-and-close", "05-enter"],
          "last_close": {},
          "break_close_at": null,
          "retest_seen_at": null,
          "mw": null,
          "expires_at": "2026-06-18T20:00:00Z"
        },
        "done": true
      },
      "shadow": false,
      "dispatch_outcomes": [
        {
          "rule_id": "05-enter",
          "intent_id": "hs-copper-c474f303-enter",
          "outcome": "Ok(entered)",
          "seq": 0
        }
      ],
      "kv": {
        "key": "plan-state:experimental:hs-copper-c474f303",
        "before": null,
        "after": null,
        "cleared_plan": true,
        "success": true,
        "error": null
      }
    }"#;

    #[test]
    fn tick_bundle_round_trips_through_json() {
        // Parse the fixture, re-serialize, parse again, and compare the two as
        // structural JSON values — equality that doesn't need `PartialEq` on the
        // `Intent` graph and is robust to float formatting.
        let bundle: TickBundle = serde_json::from_str(SAMPLE).expect("fixture parses");
        let reserialized = serde_json::to_string(&bundle).expect("serializes");
        let again: TickBundle = serde_json::from_str(&reserialized).expect("re-parses");

        let a: serde_json::Value = serde_json::to_value(&bundle).expect("to value");
        let b: serde_json::Value = serde_json::to_value(&again).expect("to value");
        assert_eq!(a, b, "TickBundle must round-trip through JSON unchanged");

        // Spot-check a few load-bearing fields survived the trip.
        assert_eq!(bundle.schema_version, TICK_BUNDLE_SCHEMA_VERSION);
        assert_eq!(bundle.correlation_id, "hs-copper-c474f303");
        assert_eq!(bundle.eval.fired.len(), 1);
        assert!(bundle.eval.done);
        assert_eq!(bundle.dispatch_outcomes[0].outcome, "Ok(entered)");
        assert!(bundle.kv.cleared_plan);
    }

    #[test]
    fn r2_key_is_under_ticks_prefix_dated_and_trade_keyed() {
        let bundle: TickBundle = serde_json::from_str(SAMPLE).expect("fixture parses");
        assert_eq!(
            bundle.r2_key(),
            "ticks/2026-06-17/2026-06-17T20:00:00+00:00-hs-copper-c474f303.json"
        );
    }
}
