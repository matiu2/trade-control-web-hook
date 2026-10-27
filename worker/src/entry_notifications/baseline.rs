//! Durable distinction between existing fills and entries observed after activation.
use chrono::{DateTime, Utc};
use trade_control_core::state::EntryAttempt;

pub(super) async fn mark(pool: &sqlx::PgPool, key: &str) -> Result<DateTime<Utc>, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO operator_notification_markers (marker_key) VALUES ($1) ON CONFLICT (marker_key) DO UPDATE SET marker_key = EXCLUDED.marker_key RETURNING observed_at")
        .bind(key).fetch_one(pool).await
}

pub(super) async fn may_sound(
    pool: &sqlx::PgPool,
    attempt: &EntryAttempt,
    activation: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    if attempt.placed_at >= activation {
        return Ok(true);
    }
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM operator_notification_markers WHERE marker_key = $1)",
    )
    .bind(super::key(attempt, "entry-pending"))
    .fetch_one(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn old_fills_stay_quiet_but_old_pending_orders_can_still_ring_after_restart() {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            return;
        };
        let store = crate::PgStateStore::connect(&url).await.unwrap();
        store.migrate().await.unwrap();
        let id = format!("test-pending-{}", Utc::now().timestamp_nanos_opt().unwrap());
        let attempt: EntryAttempt = serde_json::from_value(serde_json::json!({
            "trade_id":id, "account":"competition", "instrument":"GBPAUD", "attempt_no":1,
            "broker_order_id":"order-1", "direction":"short", "placed_at":"2026-10-16T01:30:00Z",
            "shell_time":"2026-10-16T01:30:00Z", "expires_at":"2026-10-17T01:30:00Z"
        }))
        .unwrap();
        let activation: DateTime<Utc> = "2026-10-16T02:00:00Z".parse().unwrap();
        assert!(!may_sound(store.pool(), &attempt, activation).await.unwrap());
        let key = super::super::key(&attempt, "entry-pending");
        let first = mark(store.pool(), &key).await.unwrap();
        assert_eq!(
            mark(store.pool(), &key).await.unwrap(),
            first,
            "restart must retain the original marker"
        );
        assert!(may_sound(store.pool(), &attempt, activation).await.unwrap());
        let mut other_account = attempt.clone();
        other_account.account = Some("another".into());
        assert!(
            !may_sound(store.pool(), &other_account, activation)
                .await
                .unwrap()
        );
        let mut new_attempt = attempt;
        new_attempt.placed_at = activation;
        assert!(
            may_sound(store.pool(), &new_attempt, activation)
                .await
                .unwrap()
        );
        sqlx::query("DELETE FROM operator_notification_markers WHERE marker_key = $1")
            .bind(key)
            .execute(store.pool())
            .await
            .unwrap();
    }
}
