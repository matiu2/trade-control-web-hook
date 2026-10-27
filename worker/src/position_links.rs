//! Durable accepted-order identity, independent of retry and observation rows.
use color_eyre::{Result, eyre::ensure};
use trade_control_core::position_tracking::PositionLink;

pub(crate) async fn record(pool: &sqlx::PgPool, link: &PositionLink) -> Result<()> {
    let count=sqlx::query("INSERT INTO position_links(account,order_id,trade_id,body) VALUES ($1,$2,$3,$4)
        ON CONFLICT(account,order_id) DO UPDATE SET body=position_links.body WHERE position_links.body=EXCLUDED.body")
        .bind(link.account.as_deref().unwrap_or_default()).bind(&link.broker_order_id).bind(&link.trade_id).bind(serde_json::to_value(link)?)
        .execute(pool).await?.rows_affected();
    ensure!(count == 1, "position link ownership conflict");
    Ok(())
}

pub(crate) async fn list(pool: &sqlx::PgPool) -> Result<Vec<PositionLink>> {
    let bodies: Vec<serde_json::Value> = sqlx::query_scalar("SELECT body FROM position_links")
        .fetch_all(pool)
        .await?;
    bodies
        .into_iter()
        .map(|body| serde_json::from_value(body).map_err(Into::into))
        .collect()
}

pub(crate) async fn finish_cancelled(pool: &sqlx::PgPool, links: &[PositionLink]) -> Result<()> {
    for link in links {
        ensure!(
            link.broker_position_id.is_none(),
            "a filled position cannot be resolved as an unfilled cancellation"
        );
        let count=sqlx::query("INSERT INTO position_links(account,order_id,trade_id,body,resolved) VALUES ($1,$2,$3,$4,TRUE)
            ON CONFLICT(account,order_id) DO UPDATE SET resolved=TRUE WHERE position_links.body=EXCLUDED.body")
            .bind(link.account.as_deref().unwrap_or_default()).bind(&link.broker_order_id).bind(&link.trade_id).bind(serde_json::to_value(link)?)
            .execute(pool).await?.rows_affected();
        ensure!(count == 1, "cancelled placement ownership conflict");
    }
    Ok(())
}
