//! Revisit activity-only closes when the cash ledger arrives after the close event.
use super::*;
use trade_control_core::broker_exit::{BrokerTradeExit, ExitReason};

pub(super) async fn pending(pool: &sqlx::PgPool) -> Result<Vec<PositionLink>> {
    let trades: Vec<String> = sqlx::query_scalar(
        "SELECT trade_id FROM position_observations
        WHERE observed_at > now() - interval '90 days' AND closed_exit->>'broker_reason' LIKE 'activity-%'
        AND (closed_exit->>'transaction_id' IS NULL OR closed_exit->>'reason'='unknown')
        UNION SELECT correlation_id FROM cron_notes
        WHERE ts > now() - interval '90 days' AND body->'broker_exit'->'execution'->>'broker_reason' LIKE 'activity-%'
        AND (body->'broker_exit'->'execution'->>'transaction_id' IS NULL OR body->'broker_exit'->'execution'->>'reason'='unknown')",
    )
    .fetch_all(pool)
    .await?;
    let mut links = Vec::new();
    for trade in trades {
        for observation in for_trade(pool, &trade).await? {
            let Some(exit) = observation.exit else {
                continue;
            };
            if (exit.transaction_id.is_none() || exit.reason == ExitReason::Unknown)
                && exit
                    .broker_reason
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with("activity-"))
            {
                let mut link = observation.link;
                if link.instrument.is_empty() {
                    link.instrument = instrument(pool, &link).await?.unwrap_or_default();
                }
                if !link.instrument.is_empty() {
                    links.push(link);
                }
            }
        }
    }
    Ok(links)
}

async fn instrument(pool: &sqlx::PgPool, link: &PositionLink) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT body->>'instrument' FROM trade_plan WHERE COALESCE(account,'')=$1 AND trade_id=$2
        UNION ALL SELECT body->'plan'->>'instrument' FROM archived_plan WHERE COALESCE(account,'')=$1 AND trade_id=$2 LIMIT 1")
        .bind(link.account.as_deref().unwrap_or_default()).bind(&link.trade_id).fetch_optional(pool).await?.flatten())
}

/// Add evidence without allowing delayed/partial reads to remove known fields.
pub(super) fn patch(exit: &BrokerTradeExit) -> Result<Value> {
    let mut value = serde_json::to_value(exit)?;
    if let Some(fields) = value.as_object_mut() {
        fields.retain(|key, value| {
            !value.is_null() && !(key == "reason" && exit.reason == ExitReason::Unknown)
        });
    }
    Ok(value)
}

pub(super) async fn enrich_note(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account: &str,
    link: &PositionLink,
    patch: &Value,
) -> Result<()> {
    sqlx::query("UPDATE cron_notes SET body=jsonb_set(body,'{broker_exit,execution}',body->'broker_exit'->'execution' || $4)
        WHERE COALESCE(account,'')=$1 AND correlation_id=$2 AND body->'broker_exit'->>'broker_order_id'=$3
        AND body->'broker_exit'->'execution'->>'broker_trade_id'=$5")
        .bind(account).bind(&link.trade_id).bind(&link.broker_order_id).bind(patch)
        .bind(link.broker_position_id.as_deref()).execute(&mut **tx).await?;
    Ok(())
}
