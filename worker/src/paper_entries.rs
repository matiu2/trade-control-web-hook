//! Durable paper entry notes: retries cannot lose or duplicate a consumed signal.

use serde::Serialize;
use trade_control_core::recording::CronNote;

#[derive(Serialize)]
struct PaperRecord<'a> {
    #[serde(flatten)]
    note: &'a CronNote,
    paper_entry_identity: &'a str,
}

pub(crate) async fn record(
    pool: &sqlx::PgPool,
    identity: &str,
    note: &CronNote,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let lock_key = serde_json::to_string(&(&note.account, &note.trade_id, &note.source, identity))
        .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(lock_key)
        .execute(&mut *tx)
        .await?;
    let body = serde_json::to_value(PaperRecord {
        note,
        paper_entry_identity: identity,
    })
    .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    let ts = chrono::DateTime::parse_from_rfc3339(&note.ts)
        .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    let inserted = sqlx::query(
        "INSERT INTO cron_notes (ts, correlation_id, account, source, severity, message, body)
         SELECT $1,$2,$3,$4,$5,$6,$7
         WHERE NOT EXISTS (SELECT 1 FROM cron_notes WHERE account IS NOT DISTINCT FROM $3
            AND correlation_id = $2 AND source = $4 AND body->>'paper_entry_identity' = $8)",
    )
    .bind(ts)
    .bind(&note.trade_id)
    .bind(&note.account)
    .bind(&note.source)
    .bind(note.severity.to_string())
    .bind(&note.message)
    .bind(body)
    .bind(identity)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    tx.commit().await?;
    if inserted {
        tracing::info!(
            trade_id = note.trade_id,
            message = note.message,
            "paper entry recorded"
        );
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;
    use trade_control_core::recording::CronNoteSeverity;

    fn note() -> CronNote {
        CronNote {
            ts: "2026-10-20T03:01:00Z".into(),
            trade_id: "paper-test".into(),
            account: Some("healthy-data".into()),
            source: "paper-entry".into(),
            severity: CronNoteSeverity::Info,
            message: "PAPER ENTRY: signal only".into(),
            broker_exit: None,
            attempt_resolution: None,
        }
    }

    #[test]
    fn durable_identity_is_preserved_without_changing_timeline_note_shape() {
        let note = note();
        let body = serde_json::to_value(PaperRecord {
            note: &note,
            paper_entry_identity: "rule:bar",
        })
        .unwrap();
        assert_eq!(body["paper_entry_identity"], "rule:bar");
        let decoded: CronNote = serde_json::from_value(body).unwrap();
        assert_eq!(decoded.source, "paper-entry");
        assert_eq!(decoded.message, note.message);
    }

    #[tokio::test]
    async fn concurrent_retries_record_once_and_accounts_and_signal_bars_remain_distinct() {
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer())
            .try_init()
            .ok();
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            return;
        };
        let store = crate::PgStateStore::connect(&url).await.unwrap();
        store.migrate().await.unwrap();
        let mut note = note();
        note.trade_id = format!(
            "test-paper-entry-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        );
        let (first, repeated) = tokio::join!(
            record(store.pool(), "rule:bar1", &note),
            record(store.pool(), "rule:bar1", &note)
        );
        assert_ne!(first.unwrap(), repeated.unwrap());
        note.ts = "2026-10-20T03:05:00Z".into();
        assert!(!record(store.pool(), "rule:bar1", &note).await.unwrap());
        assert!(record(store.pool(), "rule:bar2", &note).await.unwrap());
        note.account = Some("another-data-account".into());
        assert!(record(store.pool(), "rule:bar1", &note).await.unwrap());
        let notes = crate::recording_pg::cron_notes_for_trade(store.pool(), &note.trade_id)
            .await
            .unwrap();
        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0].ts, "2026-10-20T03:01:00Z");
        sqlx::query("DELETE FROM cron_notes WHERE correlation_id = $1")
            .bind(&note.trade_id)
            .execute(store.pool())
            .await
            .unwrap();
    }
}
