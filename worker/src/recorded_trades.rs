//! Read-only journal projections of accepted orders without managed plans.
use std::collections::BTreeMap;

use color_eyre::{
    Result,
    eyre::{WrapErr, ensure},
};
use serde_json::{Value, json};
use trade_control_core::{
    intent::{Action, Intent},
    recording::RequestRecord,
};

#[cfg(test)]
mod tests;

/// Only accepted broker entries qualify. A 200 can also mean dry-run or dedup.
fn from_record(record: &RequestRecord) -> Result<Option<Value>> {
    if !(200..300).contains(&record.status)
        || !(record.outcome.starts_with("placed: order=")
            || record.outcome.starts_with("entered: order="))
    {
        return Ok(None);
    }
    let intent: Intent = serde_yaml::from_str(&record.body)
        .wrap_err_with(|| format!("decode accepted order record {}", record.request_id))?;
    if intent.action != Action::Enter || intent.dry_run == Some(true) {
        return Ok(None);
    }
    let Some(trade_id) = intent.trade_id.as_deref().filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    ensure!(
        record.trade_id.as_deref() == Some(trade_id),
        "recorded order trade identity mismatch"
    );
    // This is a display envelope, deliberately not an executable TradePlan:
    // there is no trigger, engine state, registration or invented timeframe.
    Ok(Some(json!({
        "account":intent.account, "recorded_only":true,
        "plan":{
            "trade_id":trade_id, "instrument":intent.instrument,
            "direction":intent.direction, "broker":intent.broker,
            "granularity":"", "armed_at":record.ts, "recorded_only":true,
            "rules":[{"rule_id":"direct-entry", "intent":intent}]
        },
        "request":record,
    })))
}

async fn entries(pool: &sqlx::PgPool, target: Option<&str>) -> Result<Vec<Value>> {
    let bodies: Vec<Value> = sqlx::query_scalar(
        "SELECT body FROM request_records
         WHERE status >= 200 AND status < 300 AND trade_id IS NOT NULL
         AND (outcome LIKE 'placed: order=%' OR outcome LIKE 'entered: order=%')
         AND ($1::text IS NULL OR trade_id = $1)
         ORDER BY ts, id",
    )
    .bind(target)
    .fetch_all(pool)
    .await?;
    let mut entries = BTreeMap::new();
    for body in bodies {
        let record: RequestRecord = serde_json::from_value(body)?;
        if let Some(entry) = from_record(&record)? {
            let key = (
                entry["account"].as_str().unwrap_or_default().to_owned(),
                entry["plan"]["trade_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
            // Keep the original order rather than replacing its entry/SL/TP
            // with a later amendment or retry. Every request stays in timeline.
            entries.entry(key).or_insert(entry);
        }
    }
    Ok(entries.into_values().collect())
}

pub(crate) async fn list(pool: &sqlx::PgPool) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for entry in entries(pool, None).await? {
        let plan = &entry["plan"];
        let owned: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM trade_plan WHERE trade_id=$1 AND account IS NOT DISTINCT FROM $2
             UNION ALL SELECT 1 FROM archived_plan WHERE trade_id=$1 AND account IS NOT DISTINCT FROM $2)",
        ).bind(plan["trade_id"].as_str()).bind(entry["account"].as_str()).fetch_one(pool).await?;
        if !owned {
            rows.push(json!({
                "trade_id":plan["trade_id"], "account":entry["account"].as_str().unwrap_or_default(),
                "instrument":plan["instrument"], "direction":plan["direction"],
                "granularity":"", "phase":"direct order", "recorded_only":true,
                "watermark":plan["armed_at"], "shadow":false,
            }));
        }
    }
    Ok(rows)
}

pub(crate) async fn enrich_list(pool: &sqlx::PgPool, body: &str) -> Result<String> {
    let mut rows: Vec<Value> = serde_yaml::from_str(body)?;
    rows.extend(list(pool).await?);
    serde_yaml::to_string(&rows).map_err(Into::into)
}

pub(crate) async fn show(pool: &sqlx::PgPool, trade_id: &str) -> Result<Option<Value>> {
    let entries = entries(pool, Some(trade_id)).await?;
    ensure!(
        entries.len() <= 1,
        "recorded order has ambiguous account identity for {trade_id}"
    );
    Ok(entries.into_iter().next())
}
