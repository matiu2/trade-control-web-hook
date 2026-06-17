//! Per-trade engine state: the [`PlanState`] the server-side cron engine
//! evolves for each registered [`TradePlan`](crate::trade_plan::TradePlan),
//! plus the [`Phase`] of its sequential spine.
//!
//! Where the old TradingView path used *stateless* alerts (each dings on a
//! cross, blind to the trade's status, so ordering was faked with the `clears`
//! kludge — see `Intent::clears`), the engine drives **one state machine per
//! `trade_id`**. `PlanState` is the persisted half of that machine: a
//! watermark (the last candle processed), the current spine phase, per-rule
//! fire latches, and the two cross-tick memories the evaluator needs
//! (`last_close` for OnClose crosses, `retest_seen_at` for the
//! break-and-close → retest → entry ordering).
//!
//! This type lives in `core` (not the engine crate) so the
//! [`StateStore`](crate::state::StateStore) trait can name it without a
//! dependency cycle — exactly as [`MwState`](crate::state::MwState) does. The
//! pure evaluator in the engine crate imports it from here. It is plain
//! persisted data: no behaviour, no I/O.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The sequential spine the engine walks for a trade. A phase only ever
/// advances — never regresses — and a rule whose phase has passed is "dead"
/// (never re-evaluated). The starting phase is derived from which rules a
/// plan carries (see the engine's `initial_phase`): an H&S plan with a
/// `prep-break-and-close` rule starts at [`Phase::AwaitBreakAndClose`]; an M/W
/// plan (no prep, a per-bar heartbeat enter) starts at [`Phase::AwaitEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Waiting for the candle that closes through the neckline trendline.
    /// Only present for plans that carry a break-and-close prep.
    AwaitBreakAndClose,
    /// Break-and-close has fired (or wasn't required). Watching for the entry
    /// trigger, gated by the retest lookback (see [`PlanState::retest_seen_at`]).
    AwaitEntry,
    /// The entry was dispatched (or a terminal veto fired). The engine can drop
    /// the plan. Note: M/W plans can't observe broker fills, so they end by TTL
    /// or a veto rather than reaching `Done` via the spine.
    Done,
}

/// The evolving state the engine persists per `(account, trade_id)`, keyed
/// `plan-state:{scope}:{trade_id}` (mirrors the `mw-state:` keyspace).
///
/// `BTreeSet` / `BTreeMap` (not the hashed variants) are deliberate: their
/// serialization is order-stable, so the JSON KV body is deterministic and an
/// idempotent re-tick produces a byte-identical row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanState {
    /// Open-time of the last candle the engine has processed. The next fetch
    /// asks for candles strictly `> watermark` (mirrors
    /// [`filter_new_candles`](crate::broker::filter_new_candles)). `None` means
    /// the plan has never ticked — the first tick *seeds* the watermark and
    /// `last_close` **without firing** any rule (a condition already true at
    /// register time doesn't fire retroactively, matching a fresh TV alert).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<DateTime<Utc>>,
    /// The current spine phase. Only advances.
    pub phase: Phase,
    /// `rule_id`s of [`FireMode::Once`](crate::trade_plan::FireMode) rules that
    /// have fired and are now latched — the server-side win over a TV alert
    /// that re-fires on every touch. Skipped on later ticks.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub fired: BTreeSet<String>,
    /// Last processed close per `rule_id`, so an `OnClose` cross can be
    /// detected against the prior bar's close even when the two bars land in
    /// different cron ticks. Seeded (without firing) on the first tick.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_close: BTreeMap<String, f64>,
    /// Open-time of the candle on which break-and-close fired — the start of
    /// the retest lookback window. `None` until that rule fires (or for plans
    /// with no break-and-close prep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub break_close_at: Option<DateTime<Utc>>,
    /// Open-time of the most recent candle (after `break_close_at`) that
    /// satisfied the retest trendline geometry. Stamped every tick while in
    /// [`Phase::AwaitEntry`] so a retest that closed in an *earlier* tick than
    /// the entry isn't missed. The entry gate is "is `retest_seen_at` within
    /// `(break_close_at, entry]`?". `None` until a retest is seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retest_seen_at: Option<DateTime<Utc>>,
    /// Reserved for M/W neckline-evolution state. **Unused in Stage D** — the
    /// engine delegates all M/W geometry to the existing
    /// `run_enter → maybe_update_mw_state` path (one implementation), so this
    /// stays `None`. Kept so a future stage can fold the M/W decision in
    /// without a wire-format break.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mw: Option<crate::state::MwState>,
    /// Safety TTL — like [`MwState::expires_at`](crate::state::MwState), this
    /// just ages an orphaned row out; the authoritative end of a plan is its
    /// dispatch, a terminal veto, or the KV TTL on the plan itself.
    pub expires_at: DateTime<Utc>,
}

impl PlanState {
    /// A fresh, never-ticked state for `phase`. The first engine tick seeds the
    /// watermark + `last_close` from the back-window and persists this with
    /// `fired` empty, so nothing fires retroactively.
    pub fn seed(phase: Phase, expires_at: DateTime<Utc>) -> Self {
        Self {
            watermark: None,
            phase,
            fired: BTreeSet::new(),
            last_close: BTreeMap::new(),
            break_close_at: None,
            retest_seen_at: None,
            mw: None,
            expires_at,
        }
    }
}

impl crate::state::HasExpiry for PlanState {
    fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn seed_is_empty_and_unwatermarked() {
        let s = PlanState::seed(Phase::AwaitEntry, ts("2026-06-20T00:00:00Z"));
        assert!(s.watermark.is_none());
        assert_eq!(s.phase, Phase::AwaitEntry);
        assert!(s.fired.is_empty());
        assert!(s.last_close.is_empty());
        assert!(s.retest_seen_at.is_none());
        assert!(s.mw.is_none());
    }

    #[test]
    fn round_trips_through_json_with_skipped_empties() {
        // A seed state serialises compactly (empty collections + None elided),
        // and re-parses to the same value.
        let s = PlanState::seed(Phase::AwaitBreakAndClose, ts("2026-06-20T00:00:00Z"));
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("watermark"), "json was: {json}");
        assert!(!json.contains("last_close"), "json was: {json}");
        assert!(!json.contains("\"mw\""), "json was: {json}");
        let back: PlanState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn phase_wire_form_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&Phase::AwaitBreakAndClose).unwrap(),
            "\"await_break_and_close\""
        );
        assert_eq!(
            serde_json::to_string(&Phase::AwaitEntry).unwrap(),
            "\"await_entry\""
        );
        assert_eq!(serde_json::to_string(&Phase::Done).unwrap(), "\"done\"");
    }

    #[test]
    fn populated_state_round_trips() {
        let mut s = PlanState::seed(Phase::AwaitEntry, ts("2026-06-20T00:00:00Z"));
        s.watermark = Some(ts("2026-06-16T12:00:00Z"));
        s.fired.insert("03-prep-break-and-close".into());
        s.last_close.insert("01-veto-too-high".into(), 1.2345);
        s.break_close_at = Some(ts("2026-06-16T11:00:00Z"));
        s.retest_seen_at = Some(ts("2026-06-16T11:30:00Z"));
        let json = serde_json::to_string(&s).unwrap();
        let back: PlanState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
