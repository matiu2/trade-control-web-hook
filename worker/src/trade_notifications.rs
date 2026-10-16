//! Positive trade events use stable identities, so retries never celebrate twice.
use chrono::{DateTime, Utc};
use trade_control_core::{
    recording::{CronNote, CronNoteSeverity},
    trade_plan::TradePlan,
};

pub(crate) fn key(account: Option<&str>, trade_id: &str, source: &str, identity: &str) -> String {
    serde_json::json!([account, trade_id, source, identity]).to_string()
}

pub(crate) fn registered(account: Option<&str>, plan: &TradePlan, now: DateTime<Utc>) -> CronNote {
    note(
        account,
        &plan.trade_id,
        "trade-registered",
        now,
        format!(
            "{} {:?} {:?} setup registered{}",
            plan.instrument,
            plan.granularity,
            plan.direction,
            if plan.shadow { " (observe only)" } else { "" }
        ),
    )
}

pub(crate) fn note(
    account: Option<&str>,
    trade_id: &str,
    source: &str,
    now: DateTime<Utc>,
    message: String,
) -> CronNote {
    CronNote {
        ts: now.to_rfc3339(),
        trade_id: trade_id.into(),
        account: account.map(str::to_owned),
        source: source.into(),
        severity: CronNoteSeverity::Info,
        message,
        broker_exit: None,
        attempt_resolution: None,
    }
}

/// Atomically journal and queue an event once per plan registration or entry
/// order. The primary key also deduplicates concurrent observers and restarts.
pub(crate) async fn record(
    pool: &sqlx::PgPool,
    identity: &str,
    note: &CronNote,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let inserted = record_in(tx.as_mut(), identity, note).await?;
    tx.commit().await?;
    if inserted {
        tracing::info!(
            trade_id = note.trade_id,
            source = note.source,
            "trade notification queued"
        );
    }
    Ok(inserted)
}

pub(crate) async fn record_in(
    conn: &mut sqlx::PgConnection,
    identity: &str,
    note: &CronNote,
) -> Result<bool, sqlx::Error> {
    let key = key(
        note.account.as_deref(),
        &note.trade_id,
        &note.source,
        identity,
    );
    let body = serde_json::to_value(note).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    let inserted = sqlx::query("INSERT INTO operator_alert_outbox (event_key, body) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(key).bind(&body).execute(&mut *conn).await?.rows_affected() > 0;
    if inserted {
        let ts = DateTime::parse_from_rfc3339(&note.ts)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        sqlx::query("INSERT INTO cron_notes (ts, correlation_id, account, source, severity, message, body) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(ts).bind(&note.trade_id).bind(&note.account).bind(&note.source)
            .bind(note.severity.to_string()).bind(&note.message).bind(body).execute(&mut *conn).await?;
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;
    use trade_control_core::state::StateStore;
    fn plan() -> TradePlan {
        serde_json::from_value(
            serde_json::json!({"trade_id":"test-new-plan", "instrument":"GBPAUD",
            "direction":"short", "granularity":"m15", "pip_size":0.0001, "rules":[]}),
        )
        .unwrap()
    }
    #[test]
    fn registration_note_describes_the_persisted_plan() {
        let note = registered(Some("competition"), &plan(), Utc::now());
        assert_eq!(note.source, "trade-registered");
        assert_eq!(note.account.as_deref(), Some("competition"));
        assert!(note.message.contains("GBPAUD"));
    }
    #[tokio::test]
    async fn registration_and_entry_events_survive_retries_without_duplicate_notes() {
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
        let mut plan = plan();
        plan.trade_id = format!(
            "test-celebration-{}",
            Utc::now().timestamp_nanos_opt().unwrap()
        );
        let registration = registered(Some("competition"), &plan, Utc::now());
        store
            .put_trade_plan(Some("competition"), &plan)
            .await
            .unwrap();
        assert!(
            store
                .get_trade_plan(Some("competition"), &plan.trade_id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            !record(store.pool(), "registered", &registration)
                .await
                .unwrap(),
            "plan persistence must already have committed its notification"
        );
        store
            .put_trade_plan(Some("competition"), &plan)
            .await
            .unwrap();
        let entry = note(
            Some("competition"),
            &plan.trade_id,
            "trade-entered",
            Utc::now(),
            "Entry confirmed".into(),
        );
        assert!(record(store.pool(), "order-1", &entry).await.unwrap());
        assert!(!record(store.pool(), "order-1", &entry).await.unwrap());
        assert!(record(store.pool(), "order-2", &entry).await.unwrap());
        let notes = crate::recording_pg::cron_notes_for_trade(store.pool(), &plan.trade_id)
            .await
            .unwrap();
        assert_eq!(notes.len(), 3);
        store
            .clear_trade_plan(Some("competition"), &plan.trade_id)
            .await
            .unwrap();
        sqlx::query("DELETE FROM operator_alert_outbox WHERE body->>'trade_id' = $1")
            .bind(&plan.trade_id)
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM cron_notes WHERE correlation_id = $1")
            .bind(&plan.trade_id)
            .execute(store.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failed_registration_transaction_leaves_neither_plan_nor_sound() {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            return;
        };
        let store = crate::PgStateStore::connect(&url).await.unwrap();
        store.migrate().await.unwrap();
        let mut plan = plan();
        plan.trade_id = format!(
            "test-registration-rollback-{}",
            Utc::now().timestamp_nanos_opt().unwrap()
        );
        let mut event = registered(Some("competition"), &plan, Utc::now());
        event.ts = "invalid timestamp".into();
        let mut tx = store.pool().begin().await.unwrap();
        sqlx::query("INSERT INTO trade_plan (account, trade_id, body) VALUES ($1,$2,$3)")
            .bind("competition")
            .bind(&plan.trade_id)
            .bind(serde_json::to_value(&plan).unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(record_in(tx.as_mut(), "registered", &event).await.is_err());
        tx.rollback().await.unwrap();
        assert!(
            store
                .get_trade_plan(Some("competition"), &plan.trade_id)
                .await
                .unwrap()
                .is_none()
        );
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM operator_alert_outbox WHERE event_key = $1)",
        )
        .bind(key(
            Some("competition"),
            &plan.trade_id,
            "trade-registered",
            "registered",
        ))
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!(!exists, "failed registration must not celebrate");
    }
}
