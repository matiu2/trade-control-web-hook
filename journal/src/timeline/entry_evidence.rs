//! Fill evidence can arrive through HTTP, engine dispatch, or broker observers.
use chrono::{DateTime, Utc};
use serde_json::Value;

fn dispatches(value: &Value) -> Vec<(&str, &str)> {
    let records = value["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| Some((r["ts"].as_str().unwrap_or_default(), r["outcome"].as_str()?)));
    let ticks = value["ticks"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|tick| {
            tick["dispatch_outcomes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|outcome| {
                    let text = outcome["outcome"].as_str()?;
                    let text = text
                        .strip_prefix("Ok(")
                        .and_then(|s| s.strip_suffix(')'))
                        .unwrap_or(text);
                    Some((tick["tick_ts"].as_str()?, text))
                })
        });
    records.chain(ticks).collect()
}

pub(super) fn entry_ts(value: &Value) -> Option<String> {
    let positions: Vec<_> = value["positions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some((
                p,
                DateTime::parse_from_rfc3339(p["snapshot"]["position"]["opened_at"].as_str()?)
                    .ok()?,
            ))
        })
        .collect();
    let fill_time = |ts: &str, text: &str| {
        let ids: Vec<_> = text
            .split_whitespace()
            .filter_map(|word| {
                word.strip_prefix("order=")
                    .or_else(|| word.strip_prefix("position="))
            })
            .map(|id| id.trim_end_matches(';'))
            .collect();
        positions
            .iter()
            .find(|(p, _)| {
                ids.iter().any(|id| {
                    !id.is_empty()
                        && [
                            p["link"]["broker_order_id"].as_str(),
                            p["link"]["broker_position_id"].as_str(),
                            p["snapshot"]["position"]["order_id"].as_str(),
                            p["snapshot"]["position"]["position_id"].as_str(),
                        ]
                        .contains(&Some(*id))
                })
            })
            .map(|(_, at)| *at)
            .or_else(|| DateTime::parse_from_rfc3339(ts).ok())
    };
    let fills = dispatches(value)
        .into_iter()
        .filter(|(_, outcome)| outcome.starts_with("entered"))
        .filter_map(|(ts, text)| fill_time(ts, text));
    let notes = value["notes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|note| note["source"] == "trade-entered")
        .filter_map(|note| {
            fill_time(
                note["ts"].as_str()?,
                note["message"].as_str().unwrap_or_default(),
            )
        });
    fills
        .chain(notes)
        .chain(positions.iter().map(|(_, at)| *at))
        .min()
        .map(|ts| super::ts_to_bne(&ts.with_timezone(&Utc).to_rfc3339()))
}

pub(super) fn last_dispatch(value: &Value) -> Option<(String, bool)> {
    dispatches(value)
        .into_iter()
        .filter(|(_, outcome)| {
            !outcome.is_empty()
                && *outcome != "ok"
                && !outcome.starts_with("- ")
                && !outcome.contains('\n')
        })
        .max_by_key(|(ts, _)| DateTime::parse_from_rfc3339(ts).ok())
        .map(|(_, outcome)| {
            (
                outcome.into(),
                outcome.starts_with("entered")
                    || outcome.starts_with("placed")
                    || outcome.starts_with("closed"),
            )
        })
}
