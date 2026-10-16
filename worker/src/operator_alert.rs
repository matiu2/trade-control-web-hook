//! Durable notification delivery, driven independently of trading evaluations.
use std::{sync::OnceLock, time::Duration};
use trade_control_core::recording::CronNote;

fn origin() -> String {
    std::env::var("TRADE_CONTROL_ALERT_SOURCE").unwrap_or_else(|_| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "trade-control-worker".into())
    })
}

/// Call inside the observation's transaction so neither write can be lost alone.
pub(crate) async fn enqueue(
    conn: &mut sqlx::PgConnection,
    note: &CronNote,
) -> Result<(), sqlx::Error> {
    let key = serde_json::json!([note.account, note.trade_id, note.source, note.ts]).to_string();
    let body = serde_json::to_value(note).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    sqlx::query("INSERT INTO operator_alert_outbox (event_key, body) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(key).bind(body).execute(conn).await?;
    Ok(())
}

pub(crate) async fn flush(pool: &sqlx::PgPool) {
    let endpoint = std::env::var("LOCAL_CHART_ALERT_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8790/alerts".into());
    if let Err(error) = drain(pool, &endpoint).await {
        tracing::warn!(%error, "operator alert delivery failed; durable queue will retry");
    }
}

async fn drain(pool: &sqlx::PgPool, endpoint: &str) -> color_eyre::Result<()> {
    let pending: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT event_key, body FROM operator_alert_outbox WHERE delivered_at IS NULL ORDER BY last_attempt_at NULLS FIRST, created_at, event_key LIMIT 20")
        .fetch_all(pool).await?;
    for (key, body) in pending {
        if let Err(error) = deliver_one(pool, endpoint, &key, body).await {
            tracing::warn!(event_key=key, %error, "operator alert retained for retry; continuing with other notifications");
        }
    }
    Ok(())
}

async fn deliver_one(
    pool: &sqlx::PgPool,
    endpoint: &str,
    key: &str,
    mut body: serde_json::Value,
) -> color_eyre::Result<()> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(reqwest::Client::new);
    // Rotate failed deliveries behind unattempted rows, so even a permanently
    // rejected payload cannot starve subsequent incidents and recoveries.
    sqlx::query("UPDATE operator_alert_outbox SET last_attempt_at = now() WHERE event_key = $1")
        .bind(key)
        .execute(pool)
        .await?;
    if let Some(message) = body.get("message").and_then(serde_json::Value::as_str)
        && message.len() > 16_000
    {
        body["message"] = format!(
            "{}… [full details in journal]",
            message.chars().take(4000).collect::<String>()
        )
        .into();
    }
    client
        .post(endpoint)
        .timeout(Duration::from_secs(2))
        .json(&serde_json::json!({"origin": origin(), "note": body}))
        .send()
        .await?
        .error_for_status()?;
    sqlx::query("UPDATE operator_alert_outbox SET delivered_at = now() WHERE event_key = $1")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, http::StatusCode, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tracing_subscriber::prelude::*;
    use trade_control_core::recording::CronNoteSeverity;

    #[tokio::test]
    async fn failed_entry_delivery_remains_durable_until_chart_accepts_it() {
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
        let note = CronNote {
            ts: chrono::Utc::now().to_rfc3339(),
            trade_id: format!(
                "test-alert-outbox-{}",
                chrono::Utc::now().timestamp_nanos_opt().unwrap()
            ),
            account: Some("competition".into()),
            source: "entry-dispatch".into(),
            severity: CronNoteSeverity::Warn,
            message: "entry rejected".into(),
            broker_exit: None,
            attempt_resolution: None,
        };
        crate::recording_pg::record_cron_note(store.pool(), &note)
            .await
            .unwrap();
        let key =
            serde_json::json!([note.account, note.trade_id, note.source, note.ts]).to_string();
        let body = serde_json::to_value(&note).unwrap();
        let accepts = Arc::new(AtomicBool::new(false));
        let state = accepts.clone();
        let app = Router::new().route(
            "/",
            post(move || {
                let state = state.clone();
                async move {
                    if state.load(Ordering::SeqCst) {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        assert!(
            deliver_one(store.pool(), &endpoint, &key, body.clone())
                .await
                .is_err()
        );
        let pending: bool = sqlx::query_scalar(
            "SELECT delivered_at IS NULL FROM operator_alert_outbox WHERE event_key = $1",
        )
        .bind(&key)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!(pending, "failed delivery was incorrectly acknowledged");
        accepts.store(true, Ordering::SeqCst);
        deliver_one(store.pool(), &endpoint, &key, body)
            .await
            .unwrap();
        let pending: bool = sqlx::query_scalar(
            "SELECT delivered_at IS NULL FROM operator_alert_outbox WHERE event_key = $1",
        )
        .bind(&key)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!(!pending);
        server.abort();
        sqlx::query("DELETE FROM operator_alert_outbox WHERE event_key = $1")
            .bind(key)
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM cron_notes WHERE correlation_id = $1")
            .bind(note.trade_id)
            .execute(store.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rejected_backlog_does_not_starve_later_alerts() {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            return;
        };
        let store = crate::PgStateStore::connect(&url).await.unwrap();
        store.migrate().await.unwrap();
        let prefix = format!(
            "test-rejected-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        );
        let good_id = format!("{prefix}-good");
        let accepted_id = good_id.clone();
        let app = Router::new().route(
            "/",
            post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let accepted_id = accepted_id.clone();
                async move {
                    if body["note"]["trade_id"] == accepted_id
                        && body["note"]["message"].as_str().unwrap().len() < 16_384
                    {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::BAD_REQUEST
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        for index in 0..22 {
            let note = CronNote {
                ts: chrono::Utc::now().to_rfc3339(),
                trade_id: if index == 21 {
                    good_id.clone()
                } else {
                    format!("{prefix}-{index}")
                },
                account: None,
                source: "entry-dispatch".into(),
                severity: CronNoteSeverity::Warn,
                message: "x".repeat(20_000),
                broker_exit: None,
                attempt_resolution: None,
            };
            crate::recording_pg::record_cron_note(store.pool(), &note)
                .await
                .unwrap();
        }
        drain(store.pool(), &endpoint).await.unwrap();
        drain(store.pool(), &endpoint).await.unwrap();
        let delivered: bool = sqlx::query_scalar("SELECT delivered_at IS NOT NULL FROM operator_alert_outbox WHERE body->>'trade_id' = $1")
            .bind(&good_id).fetch_one(store.pool()).await.unwrap();
        assert!(
            delivered,
            "permanently rejected payloads starved the later alert"
        );
        server.abort();
        sqlx::query("DELETE FROM operator_alert_outbox WHERE body->>'trade_id' LIKE $1")
            .bind(format!("{prefix}%"))
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM cron_notes WHERE correlation_id LIKE $1")
            .bind(format!("{prefix}%"))
            .execute(store.pool())
            .await
            .unwrap();
    }
}
