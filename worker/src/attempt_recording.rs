//! Durable, change-only attempt evidence; the journal reads ordinary CronNotes.
use chrono::{DateTime, Utc};
use trade_control_core::{
    attempt_resolution::Diagnostic,
    recording::{CronNote, CronNoteSeverity},
    state::EntryAttempt,
};

pub async fn record(
    pool: &sqlx::PgPool,
    attempt: &EntryAttempt,
    diagnostic: &Diagnostic,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let value = serde_json::to_value(diagnostic).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    let key = serde_json::to_string(&(
        attempt.account.as_deref(),
        &attempt.trade_id,
        &attempt.broker_order_id,
    ))
    .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    let mut tx = pool.begin().await?;
    // Serialize only writers for this account/plan/order. Failed inserts roll
    // back, so a diagnostic is never marked recorded before it is durable.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(&mut *tx)
        .await?;
    let previous: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT body->'attempt_resolution' FROM cron_notes WHERE account IS NOT DISTINCT FROM $1 AND correlation_id = $2 AND source = 'attempt-resolution' AND body->'attempt_resolution'->>'order_id' = $3 ORDER BY id DESC LIMIT 1")
        .bind(attempt.account.as_deref()).bind(&attempt.trade_id).bind(&attempt.broker_order_id).fetch_optional(&mut *tx).await?;
    if previous.as_ref() == Some(&value) {
        tx.commit().await?;
        return Ok(false);
    }
    let message = format!(
        "attempt #{} order={} position={} state={} reason={} history_complete={} candidates={}",
        attempt.attempt_no,
        diagnostic.order_id,
        diagnostic
            .resolved_position_id
            .as_deref()
            .unwrap_or("unknown"),
        diagnostic.state,
        diagnostic.reason,
        diagnostic.history_complete,
        serde_json::to_string(&diagnostic.candidates)
            .map_err(|e| sqlx::Error::Encode(Box::new(e)))?
    );
    let note = CronNote {
        ts: now.to_rfc3339(),
        trade_id: attempt.trade_id.clone(),
        account: attempt.account.clone(),
        source: "attempt-resolution".into(),
        severity: if matches!(diagnostic.state.as_str(), "unknown" | "lookup-failed") {
            CronNoteSeverity::Warn
        } else {
            CronNoteSeverity::Info
        },
        message,
        broker_exit: None,
        attempt_resolution: Some(diagnostic.clone()),
    };
    let body = serde_json::to_value(&note).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query("INSERT INTO cron_notes (ts, correlation_id, account, source, severity, message, body) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(now).bind(&note.trade_id).bind(note.account.as_deref()).bind(&note.source).bind(note.severity.to_string()).bind(&note.message).bind(body)
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}
