//! Durable incident transitions, with the original note retained for delivery retries.
use trade_control_core::recording::{CronNote, CronNoteSeverity};

fn changed(previous: Option<&CronNote>, next: &CronNote) -> bool {
    match previous {
        None => next.severity != CronNoteSeverity::Info,
        Some(previous) => previous.severity != next.severity || previous.message != next.message,
    }
}

/// Same failure: return its original note. Recovery: append once. Healthy
/// plans without an incident produce no journal noise. The transaction lock
/// also protects against overlapping scheduler invocations.
pub(crate) async fn record(
    pool: &sqlx::PgPool,
    next: &CronNote,
) -> Result<Option<CronNote>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let key = serde_json::to_string(&(&next.account, &next.trade_id, &next.source))
        .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(&mut *tx)
        .await?;
    let previous: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT body FROM cron_notes WHERE account IS NOT DISTINCT FROM $1 AND correlation_id = $2 AND source = $3 ORDER BY id DESC LIMIT 1")
        .bind(&next.account).bind(&next.trade_id).bind(&next.source).fetch_optional(&mut *tx).await?;
    let previous = previous
        .map(serde_json::from_value::<CronNote>)
        .transpose()
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
    let result = if changed(previous.as_ref(), next) {
        let ts = chrono::DateTime::parse_from_rfc3339(&next.ts)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let body =
            serde_json::to_value(next).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        sqlx::query("INSERT INTO cron_notes (ts, correlation_id, account, source, severity, message, body) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(ts).bind(&next.trade_id).bind(&next.account).bind(&next.source)
            .bind(next.severity.to_string()).bind(&next.message).bind(body).execute(&mut *tx).await?;
        crate::operator_alert::enqueue(tx.as_mut(), next).await?;
        tracing::info!(
            trade_id = next.trade_id,
            message = next.message,
            "engine-health transition recorded"
        );
        Some(next.clone())
    } else {
        previous
    };
    tx.commit().await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn note(severity: CronNoteSeverity, message: &str) -> CronNote {
        CronNote {
            ts: "2026-10-16T01:08:24Z".into(),
            account: Some("the5ers-competition".into()),
            trade_id: "hs-gbpaud-test".into(),
            source: "engine-health".into(),
            severity,
            message: message.into(),
            broker_exit: None,
            attempt_resolution: None,
        }
    }
    #[test]
    fn incidents_record_onset_cause_change_recovery_and_recurrence() {
        use CronNoteSeverity::{Error, Info};
        let failed = note(Error, "MT5 account/server mismatch");
        let recovered = note(Info, "evaluation resumed");
        assert!(!changed(None, &recovered));
        assert!(changed(None, &failed));
        assert!(!changed(Some(&failed), &failed));
        assert!(changed(
            Some(&failed),
            &note(Error, "terminal disconnected")
        ));
        assert!(changed(Some(&failed), &recovered));
        assert!(!changed(Some(&recovered), &recovered));
        assert!(changed(Some(&recovered), &failed));
    }

    #[tokio::test]
    async fn postgres_retries_share_one_incident_and_recovery_is_durable() {
        use tracing_subscriber::prelude::*;
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
        let mut failed = note(CronNoteSeverity::Error, "MT5 account/server mismatch");
        failed.trade_id = format!(
            "test-engine-health-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        );
        let first = record(store.pool(), &failed).await.unwrap().unwrap();
        failed.ts = "2026-10-16T01:08:39Z".into();
        let retry = record(store.pool(), &failed).await.unwrap().unwrap();
        assert_eq!(
            retry.ts, first.ts,
            "retries must retain the notification identity"
        );
        let mut recovered = failed.clone();
        recovered.severity = CronNoteSeverity::Info;
        recovered.message = "evaluation resumed".into();
        recovered.ts = "2026-10-16T04:23:37Z".into();
        record(store.pool(), &recovered).await.unwrap();
        record(store.pool(), &recovered).await.unwrap();
        let notes = crate::recording_pg::cron_notes_for_trade(store.pool(), &failed.trade_id)
            .await
            .unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[1].severity, CronNoteSeverity::Info);
        // Account scope is part of the incident key.
        failed.account = Some("other-account".into());
        record(store.pool(), &failed).await.unwrap();
        assert_eq!(
            crate::recording_pg::cron_notes_for_trade(store.pool(), &failed.trade_id)
                .await
                .unwrap()
                .len(),
            3
        );
        sqlx::query("DELETE FROM cron_notes WHERE correlation_id = $1")
            .bind(&failed.trade_id)
            .execute(store.pool())
            .await
            .unwrap();
    }
}
