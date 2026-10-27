//! Machine-readable replay fires are the comparison source; report text is prose.
use super::{ReplayComparison, evidence::compare_timeline_with_replay};
use crate::{
    divergence::{self, FireFact},
    timeline::ts_to_bne,
};
use color_eyre::eyre::{Result, WrapErr, ensure, eyre};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Fire {
    rule_id: String,
    action: String,
    candle_time: chrono::DateTime<chrono::Utc>,
}

pub fn compare_timeline_with_replay_result(
    timeline: &Value,
    replay: &Value,
) -> Result<ReplayComparison> {
    ensure!(
        replay["success"].as_bool() == Some(true),
        "replay did not succeed"
    );
    let report = replay["report"]
        .as_str()
        .ok_or_else(|| eyre!("replay has no report"))?;
    let mut comparison = compare_timeline_with_replay(timeline, report)?;
    let structured = if replay["outcome"]["fires"].is_array() {
        &replay["outcome"]
    } else {
        replay["executions"]
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .rev()
                    .find_map(|item| item.get("replay_outcome"))
            })
            .ok_or_else(|| eyre!("replay has no structured outcome"))?
    };
    let fires: Vec<Fire> = serde_json::from_value(structured["fires"].clone())
        .wrap_err("replay has no valid structured fires")?;
    comparison.replay_fires = fires
        .into_iter()
        .map(|fire| FireFact {
            rule_id: fire.rule_id,
            action: Some(fire.action),
            ts: ts_to_bne(&fire.candle_time.to_rfc3339()),
            lateness_secs: None,
        })
        .collect();
    comparison.divergences = divergence::diff(&comparison.live_fires, &comparison.replay_fires);
    comparison.replay_open_at_end = structured["outcome"]["open_at_end"]
        .as_u64()
        .map(|n| n as usize);
    if let Some(warnings) = replay.get("position_warnings") {
        comparison.replay_position_warnings = serde_json::from_value(warnings.clone())
            .wrap_err("invalid replay position warnings")?;
    }
    if let Some(positions) = replay.get("open_positions") {
        comparison.replay_open_positions = serde_json::from_value(positions.clone())
            .wrap_err("invalid replay position observations")?;
    }
    Ok(comparison)
}
