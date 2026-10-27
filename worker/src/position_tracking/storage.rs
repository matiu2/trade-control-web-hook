use chrono::{DateTime, Utc};
use color_eyre::Result;
use serde_json::Value;
use trade_control_core::position_tracking::{
    PositionLink, PositionObservation, PositionSync, TrackedPosition,
};

// Closing evidence always wins over an older open snapshot. Missing from a
// snapshot alone is not a close; broker outages do not erase observed positions.
const JOIN_EXIT: &str = "LEFT JOIN LATERAL (
 SELECT body->'broker_exit'->'execution' AS execution FROM cron_notes n
 WHERE COALESCE(n.account,'')=p.account AND n.correlation_id=p.trade_id
 AND n.body->'broker_exit'->>'broker_order_id'=p.order_id
 ORDER BY n.ts DESC LIMIT 1) e ON TRUE";

pub(super) async fn candidate_links(pool: &sqlx::PgPool) -> Result<Vec<PositionLink>> {
    let mut links = active_links(pool).await?;
    let placements: Vec<Value> = sqlx::query_scalar(
        "SELECT l.body FROM position_links l
        WHERE NOT l.resolved AND NOT EXISTS(SELECT 1 FROM cron_notes n WHERE COALESCE(n.account,'')=l.account
          AND n.correlation_id=l.trade_id AND n.body->'broker_exit'->>'broker_order_id'=l.order_id)
        AND NOT EXISTS(SELECT 1 FROM position_observations p WHERE p.account=l.account
          AND p.trade_id=l.trade_id AND p.order_id=l.order_id AND p.closed_exit IS NOT NULL)",
    )
    .fetch_all(pool)
    .await?;
    for body in placements {
        append_link(&mut links, serde_json::from_value(body)?)?;
    }
    let attempts: Vec<Value> = sqlx::query_scalar("SELECT a.body FROM entry_attempt a
        WHERE NOT EXISTS (SELECT 1 FROM cron_notes n WHERE COALESCE(n.account,'')=COALESCE(a.account,'')
          AND n.correlation_id=a.trade_id AND n.body->'broker_exit'->>'broker_order_id'=a.body->>'broker_order_id')
        AND NOT EXISTS(SELECT 1 FROM position_links l WHERE l.account=COALESCE(a.account,'')
          AND l.trade_id=a.trade_id AND l.order_id=a.body->>'broker_order_id' AND l.resolved)
        AND NOT EXISTS (SELECT 1 FROM position_observations p WHERE p.account=COALESCE(a.account,'')
          AND p.trade_id=a.trade_id AND p.order_id=a.body->>'broker_order_id' AND p.closed_exit IS NOT NULL)")
        .fetch_all(pool).await?;
    for body in attempts {
        let attempt = serde_json::from_value::<trade_control_core::state::EntryAttempt>(body)?;
        let link = PositionLink::from(&attempt);
        append_link(&mut links, link)?;
    }
    Ok(links)
}

fn append_link(links: &mut Vec<PositionLink>, link: PositionLink) -> Result<()> {
    if let Some(existing) = links.iter_mut().find(|existing| {
        existing.account == link.account
            && existing.trade_id == link.trade_id
            && existing.broker_order_id == link.broker_order_id
    }) {
        color_eyre::eyre::ensure!(
            existing.instrument == link.instrument && existing.direction == link.direction,
            "conflicting instrument/direction for broker order {}",
            link.broker_order_id
        );
        if let (Some(old), Some(new)) = (&existing.broker_position_id, &link.broker_position_id) {
            color_eyre::eyre::ensure!(
                old == new,
                "conflicting position IDs for broker order {}",
                link.broker_order_id
            );
        }
        if existing.broker_position_id.is_none() {
            existing.broker_position_id = link.broker_position_id;
        }
    } else {
        links.push(link);
    }
    Ok(())
}

pub(super) async fn active_links(pool: &sqlx::PgPool) -> Result<Vec<PositionLink>> {
    let mut query = sqlx::QueryBuilder::new("SELECT p.body->'link' FROM position_observations p ");
    query
        .push(JOIN_EXIT)
        .push(" WHERE e.execution IS NULL AND p.closed_exit IS NULL");
    let bodies: Vec<Value> = query.build_query_scalar().fetch_all(pool).await?;
    bodies
        .into_iter()
        .map(|body| serde_json::from_value(body).map_err(Into::into))
        .collect()
}

pub(super) async fn record(
    pool: &sqlx::PgPool,
    account: Option<&str>,
    now: DateTime<Utc>,
    positions: &[TrackedPosition],
    exits: &[(
        PositionLink,
        trade_control_core::broker_exit::BrokerTradeExit,
    )],
    warnings: &[String],
) -> Result<()> {
    let account_key = account.unwrap_or_default();
    let mut tx = pool.begin().await?;
    // Serialize updates for an account, including a second worker polling it.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 418))")
        .bind(account_key)
        .execute(&mut *tx)
        .await?;
    let newer: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM position_sync WHERE account=$1 AND checked_at > $2)",
    )
    .bind(account_key)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;
    if newer {
        return Ok(());
    }
    sqlx::query(
        "UPDATE position_observations SET missing=TRUE WHERE account=$1 AND observed_at <= $2",
    )
    .bind(account_key)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    for tracked in positions {
        color_eyre::eyre::ensure!(
            tracked.link.account.as_deref() == account,
            "position account mismatch"
        );
        let rows = sqlx::query("INSERT INTO position_observations (account,position_id,trade_id,order_id,observed_at,body)
            VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (account,position_id) DO UPDATE
            SET body=EXCLUDED.body,observed_at=EXCLUDED.observed_at,missing=FALSE
            WHERE position_observations.trade_id=EXCLUDED.trade_id AND position_observations.observed_at <= EXCLUDED.observed_at")
            .bind(account_key).bind(&tracked.snapshot.position.position_id).bind(&tracked.link.trade_id)
            .bind(&tracked.link.broker_order_id).bind(now).bind(serde_json::to_value(tracked)?).execute(&mut *tx).await?.rows_affected();
        color_eyre::eyre::ensure!(
            rows == 1,
            "position ownership conflict or out-of-order observation"
        );
    }
    for (link, exit) in exits {
        color_eyre::eyre::ensure!(link.account.as_deref() == account, "exit account mismatch");
        color_eyre::eyre::ensure!(
            link.broker_position_id.as_deref() == Some(&exit.broker_trade_id),
            "exit position mismatch"
        );
        let rows = sqlx::query("INSERT INTO position_observations (account,position_id,trade_id,order_id,observed_at,body,closed_exit)
            VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (account,position_id) DO UPDATE SET closed_exit=EXCLUDED.closed_exit
            WHERE position_observations.trade_id=EXCLUDED.trade_id AND position_observations.order_id=EXCLUDED.order_id")
            .bind(account_key).bind(&exit.broker_trade_id).bind(&link.trade_id).bind(&link.broker_order_id).bind(now)
            .bind(serde_json::json!({"link":link,"snapshot":null})).bind(serde_json::to_value(exit)?)
            .execute(&mut *tx).await?.rows_affected();
        color_eyre::eyre::ensure!(rows == 1, "closed position ownership conflict");
    }
    let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
    write_sync(&mut tx, account_key, now, warning.as_deref()).await?;
    tx.commit().await?;
    Ok(())
}

async fn write_sync(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account: &str,
    now: DateTime<Utc>,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO position_sync(account,checked_at,error) VALUES ($1,$2,$3)
        ON CONFLICT(account) DO UPDATE SET checked_at=EXCLUDED.checked_at,error=EXCLUDED.error
        WHERE position_sync.checked_at <= EXCLUDED.checked_at",
    )
    .bind(account)
    .bind(now)
    .bind(error)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) async fn failure(
    pool: &sqlx::PgPool,
    account: Option<&str>,
    now: DateTime<Utc>,
    error: &str,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    write_sync(&mut tx, account.unwrap_or_default(), now, Some(error)).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn for_trade(
    pool: &sqlx::PgPool,
    trade: &str,
) -> Result<Vec<PositionObservation>> {
    let mut query = sqlx::QueryBuilder::new("SELECT p.body || jsonb_build_object('observed_at',p.observed_at,'status',
        CASE WHEN COALESCE(e.execution,p.closed_exit) IS NOT NULL THEN 'closed' WHEN p.missing THEN 'unconfirmed' ELSE 'open' END,
        'exit',COALESCE(e.execution,p.closed_exit)) FROM position_observations p ");
    query
        .push(JOIN_EXIT)
        .push(" WHERE p.trade_id=")
        .push_bind(trade)
        .push(" ORDER BY p.observed_at,p.position_id");
    let bodies: Vec<Value> = query.build_query_scalar().fetch_all(pool).await?;
    let mut positions: Vec<PositionObservation> = bodies
        .into_iter()
        .map(|body| serde_json::from_value(body).map_err(Into::into))
        .collect::<Result<_>>()?;
    // Reconciliation may see a rapid fill+close before this observer ever
    // sees it open. Project its existing proof, without another broker call.
    let closed: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object(
        'link',jsonb_build_object('account',n.account,'trade_id',n.correlation_id,
          'instrument',COALESCE(a.body->>'instrument',''),'direction',n.body->'broker_exit'->'direction',
          'broker_order_id',n.body->'broker_exit'->>'broker_order_id',
          'broker_position_id',n.body->'broker_exit'->'execution'->>'broker_trade_id'),
        'snapshot',NULL,'observed_at',n.ts,'status','closed','exit',n.body->'broker_exit'->'execution')
        FROM cron_notes n LEFT JOIN entry_attempt a ON COALESCE(a.account,'')=COALESCE(n.account,'')
          AND a.trade_id=n.correlation_id AND a.body->>'broker_order_id'=n.body->'broker_exit'->>'broker_order_id'
        WHERE n.correlation_id=$1 AND n.body->'broker_exit'->'execution' IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM position_observations p WHERE p.account=COALESCE(n.account,'')
          AND p.trade_id=n.correlation_id AND (p.order_id=n.body->'broker_exit'->>'broker_order_id'
          OR p.position_id=n.body->'broker_exit'->'execution'->>'broker_trade_id'))")
        .bind(trade).fetch_all(pool).await?;
    positions.extend(
        closed
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<_>, _>>()?,
    );
    positions.sort_by_key(|position| position.observed_at);
    Ok(positions)
}

#[cfg(test)]
mod tests;

pub(crate) async fn sync_for_trade(pool: &sqlx::PgPool, trade: &str) -> Result<Vec<PositionSync>> {
    let rows: Vec<(String,DateTime<Utc>,Option<String>)> = sqlx::query_as("SELECT s.account,s.checked_at,s.error FROM position_sync s
        WHERE EXISTS (SELECT 1 FROM position_observations p WHERE p.account=s.account AND p.trade_id=$1)
        OR EXISTS (SELECT 1 FROM entry_attempt a WHERE COALESCE(a.account,'')=s.account AND a.trade_id=$1)
        OR EXISTS (SELECT 1 FROM position_links l WHERE l.account=s.account AND l.trade_id=$1)")
        .bind(trade).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(account, checked_at, error)| PositionSync {
            account: (!account.is_empty()).then_some(account),
            checked_at,
            error,
        })
        .collect())
}

pub(crate) async fn enrich_list(pool: &sqlx::PgPool, body: &str) -> Result<String> {
    let mut rows: Vec<Value> = serde_yaml::from_str(body)?;
    for row in &mut rows {
        if let Some(trade) = row["trade_id"].as_str() {
            let account = row["account"].as_str();
            let positions: Vec<_> = for_trade(pool, trade)
                .await?
                .into_iter()
                .filter(|p| p.link.account.as_deref() == account)
                .collect();
            let sync: Vec<_> = sync_for_trade(pool, trade)
                .await?
                .into_iter()
                .filter(|s| s.account.as_deref() == account)
                .collect();
            row["positions"] = serde_json::to_value(positions)?;
            row["position_sync"] = serde_json::to_value(sync)?;
        }
    }
    Ok(serde_yaml::to_string(&rows)?)
}
