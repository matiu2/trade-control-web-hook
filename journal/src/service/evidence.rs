//! Serializable view models backed by the original worker evidence.

use color_eyre::eyre::{Result, WrapErr, eyre};
use serde::Serialize;
use serde_json::Value;
use trade_control_core::recording::PlanTimeline;

use crate::divergence::{self, Divergences, FireFact, ReplayOutcome};
use crate::plan::{PlanDetail, parse_plan_export};
use crate::timeline::{self, Event};

/// Full plan-show response and the single exact stored plan it identifies.
#[derive(Debug, Clone, Serialize)]
pub struct StoredPlan {
    pub raw: Value,
    pub plan: Value,
    pub detail: PlanDetail,
    pub settlement_lines: Vec<String>,
}

impl StoredPlan {
    /// Decode already-fetched YAML/JSON without discarding state or settlement.
    pub fn from_value(trade_id: &str, raw: Value) -> Result<Self> {
        let entries = raw
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_else(|| std::slice::from_ref(&raw));
        let matches: Vec<_> = entries
            .iter()
            .filter_map(|entry| Some((entry, entry.get("plan")?)))
            .filter(|(_, plan)| plan.get("trade_id").and_then(Value::as_str) == Some(trade_id))
            .collect();
        let [(entry, plan)] = matches.as_slice() else {
            return Err(eyre!(
                "plan-show expected one exact match for {trade_id}, found {}",
                matches.len()
            ));
        };
        let plan = (*plan).clone();
        let detail = parse_plan_export(&serde_json::to_string(&plan)?)?;
        let settlement_lines = timeline::settlement_lines(&serde_json::to_string(entry)?);
        Ok(Self {
            raw,
            plan,
            detail,
            settlement_lines,
        })
    }
}

/// The complete available recording plus existing journal display derivations.
/// Rendered events are a summary; `raw` retains logs and ticks with no fires.
#[derive(Debug, Clone, Serialize)]
pub struct RecordedTimeline {
    pub raw: Value,
    pub events: Vec<Event>,
    pub fires: Vec<FireFact>,
    pub entry_ts: Option<String>,
    pub outcome: String,
    pub outcome_good: bool,
}

impl RecordedTimeline {
    pub fn from_value(raw: Value) -> Result<Self> {
        validate_timeline(&raw)?;
        let json = serde_json::to_string(&raw).wrap_err("encode journal timeline evidence")?;
        let events = timeline::parse_events(&json);
        let fires = divergence::live_fires(&json);
        let entry_ts = timeline::derive_entry_ts(&json);
        let (outcome, outcome_good) = timeline::derive_outcome(&json);
        Ok(Self {
            raw,
            events,
            fires,
            entry_ts,
            outcome,
            outcome_good,
        })
    }
}

/// A rule-fire/timing comparison, not proof of dispatch or economic equality.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayComparison {
    pub live_fires: Vec<FireFact>,
    pub replay_fires: Vec<FireFact>,
    pub outcome: ReplayOutcome,
    pub divergences: Divergences,
}

/// Compare available recorded fires with the explicitly selected replay report.
pub fn compare_timeline_with_replay(timeline: &Value, report: &str) -> Result<ReplayComparison> {
    let recorded = validate_timeline(timeline)?;
    if recorded.ticks.is_empty() {
        return Err(eyre!(
            "rule-fire comparison unavailable: no recorded engine ticks are available"
        ));
    }
    let live_fires = divergence::live_fires(&serde_json::to_string(timeline)?);
    let replay_fires = divergence::parse_replay_fires(report);
    let outcome = divergence::parse_replay_outcome(report);
    validate_replay_outcome(report, &outcome)?;
    let divergences = divergence::diff(&live_fires, &replay_fires);
    Ok(ReplayComparison {
        live_fires,
        replay_fires,
        outcome,
        divergences,
    })
}

fn validate_replay_outcome(report: &str, outcome: &ReplayOutcome) -> Result<()> {
    let summary = report
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("Done:"))
        .ok_or_else(|| eyre!("replay report has no outcome summary"))?;
    let failed = summary
        .split('|')
        .any(|segment| segment.trim_start().starts_with("error:"));
    let has_phase = outcome
        .final_phase
        .as_deref()
        .is_some_and(|phase| !phase.trim().is_empty());
    if failed || outcome.done.is_none() || !has_phase || outcome.fires.is_none() {
        return Err(eyre!(
            "replay report has a failed or incomplete outcome summary"
        ));
    }
    Ok(())
}

fn validate_timeline(raw: &Value) -> Result<PlanTimeline> {
    serde_json::from_value(raw.clone()).wrap_err("decode recorded journal timeline")
}
