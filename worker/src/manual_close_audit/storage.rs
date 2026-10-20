use chrono::Utc;
use color_eyre::Result;
use serde_json::{Value, json};
use sqlx::PgPool;
use trade_control_core::recording::{CronNote, CronNoteSeverity};

use super::history::Audit;

pub(super) struct Job {
    id: i64,
    pub account: Option<String>,
    trade_id: String,
    pub body: Value,
}

// Claim all pending notes for one account. The account-level cooldown prevents
// an expired login generating a new connection failure for every detached plan.
pub(super) async fn claim(pool: &PgPool) -> Result<Vec<Job>> {
    let rows: Vec<(i64, Option<String>, String, Value)> = sqlx::query_as(
        "WITH candidate AS (
           SELECT n.account FROM cron_notes n
           WHERE n.source = 'plan-admin' AND n.body->'admin_snapshot'->>'audit_status' = 'pending'
           AND NOT EXISTS (
             SELECT 1 FROM cron_notes recent WHERE recent.source = 'plan-admin'
             AND recent.account IS NOT DISTINCT FROM n.account
             AND (recent.body->'admin_snapshot'->>'audit_attempted_at')::timestamptz > now() - interval '30 minutes'
           ) ORDER BY n.id LIMIT 1
         ) UPDATE cron_notes n SET body = jsonb_set(n.body, '{admin_snapshot,audit_attempted_at}', to_jsonb(now()))
           FROM candidate c WHERE n.source = 'plan-admin' AND n.account IS NOT DISTINCT FROM c.account
           AND n.body->'admin_snapshot'->>'audit_status' = 'pending'
           RETURNING n.id, n.account, n.correlation_id, n.body",
    ).fetch_all(pool).await?;
    let mut jobs: Vec<_> = rows
        .into_iter()
        .map(|(id, account, trade_id, body)| Job {
            id,
            account,
            trade_id,
            body,
        })
        .collect();
    oldest_first(&mut jobs);
    Ok(jobs)
}

fn oldest_first(jobs: &mut [Job]) {
    jobs.sort_by_key(|job| {
        job.body["admin_snapshot"]["audit_checked_at"]
            .as_str()
            .map(str::to_owned)
    });
}

pub(super) async fn finish(pool: &PgPool, job: &Job, result: Audit) -> Result<()> {
    let mut transaction = pool.begin().await?;
    let current: Option<Value> =
        sqlx::query_scalar("SELECT body FROM cron_notes WHERE id = $1 FOR UPDATE")
            .bind(job.id)
            .fetch_optional(transaction.as_mut())
            .await?;
    let Some(mut body) = current.filter(|body| body["admin_snapshot"]["audit_status"] == "pending")
    else {
        return Ok(());
    };
    let snapshot = &mut body["admin_snapshot"];
    snapshot["audit_checked_at"] = json!(Utc::now().to_rfc3339());
    let (status, detail) = match result {
        Audit::Pending(error) => ("pending", error),
        Audit::Unavailable(reason) => ("unavailable", reason),
        Audit::Observed(settlement) => {
            let (status, detail) = super::history::summary(&settlement);
            if status != "pending" {
                let note = CronNote {
                    ts: Utc::now().to_rfc3339(),
                    trade_id: job.trade_id.clone(),
                    account: job.account.clone(),
                    source: "manual-close-audit".into(),
                    severity: CronNoteSeverity::Info,
                    message: format!("Manual closure audit: {detail}"),
                    broker_exit: None,
                    attempt_resolution: None,
                };
                let mut note_body = serde_json::to_value(&note)?;
                note_body["settlement"] = json!(settlement);
                sqlx::query("INSERT INTO cron_notes (ts, correlation_id, account, source, severity, message, body) VALUES ($1,$2,$3,$4,$5,$6,$7)")
                    .bind(Utc::now()).bind(&note.trade_id).bind(&note.account).bind(&note.source)
                    .bind(note.severity.to_string()).bind(&note.message).bind(note_body)
                    .execute(transaction.as_mut()).await?;
            }
            sqlx::query("UPDATE archived_plan SET body = jsonb_set(body, '{settlement}', $3) WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2 AND body->'final_state'->'fired' ? 'operator:closed-manually' AND ($4 OR body->'settlement' IS NULL OR body->'settlement' = 'null'::jsonb)")
                .bind(&job.account).bind(&job.trade_id).bind(json!(settlement)).bind(status == "confirmed").execute(transaction.as_mut()).await?;
            snapshot["broker_settlement"] = json!(settlement);
            (status, detail)
        }
    };
    snapshot["audit_status"] = json!(status);
    snapshot["audit_detail"] = json!(detail);
    let original: CronNote = serde_json::from_value(job.body.clone())?;
    let original_message = original
        .message
        .split("; broker history audit:")
        .next()
        .unwrap_or(&original.message);
    let message = format!("{original_message}; broker history audit: {status}: {detail}");
    body["message"] = json!(message);
    sqlx::query("UPDATE cron_notes SET body = $2, message = $3 WHERE id = $1")
        .bind(job.id)
        .bind(body)
        .bind(message)
        .execute(transaction.as_mut())
        .await?;
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
