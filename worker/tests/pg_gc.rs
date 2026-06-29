//! Integration test for `PgStateStore::gc_expired` — the native TTL GC that
//! stands in for KV's automatic eviction. Inserts one already-expired and one
//! still-live `seen` row directly (to control `expires_at` exactly), runs the
//! GC, and asserts only the expired row is physically gone.
//!
//! Connection comes from `TEST_DATABASE_URL`, defaulting to the local dev DB.
//! Unique ids keep concurrent/repeat runs from colliding on the shared dev db.

use chrono::Utc;
use trade_control_worker::PgStateStore;

fn test_db_url() -> String {
    std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgresql://candle_cache:candle_cache@localhost:5432/trade_control_dev".to_string()
    })
}

async fn store() -> PgStateStore {
    let store = PgStateStore::connect(&test_db_url())
        .await
        .expect("connect to test db");
    store.migrate().await.expect("run migrations");
    store
}

/// Whether a `seen` row physically exists, ignoring the `expires_at > now()`
/// read filter — so we can tell "GC deleted it" from "merely expired".
async fn row_exists(store: &PgStateStore, id: &str) -> bool {
    let row: Option<(bool,)> = sqlx::query_as("SELECT true FROM seen WHERE id = $1")
        .bind(id)
        .fetch_optional(store.pool())
        .await
        .expect("query seen row");
    row.is_some()
}

#[tokio::test]
async fn gc_expired_deletes_expired_keeps_live() {
    let store = store().await;
    let nonce = Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let expired_id = format!("test-gc-expired-{nonce}");
    let live_id = format!("test-gc-live-{nonce}");

    // Insert one row already past its TTL and one comfortably live, controlling
    // `expires_at` directly so the GC's `expires_at < now()` predicate is exact.
    sqlx::query(
        "INSERT INTO seen (id, action, seen_at, outcome, expires_at) \
         VALUES ($1, 'enter', now() - interval '2 hours', 'test', now() - interval '1 hour')",
    )
    .bind(&expired_id)
    .execute(store.pool())
    .await
    .expect("insert expired row");

    sqlx::query(
        "INSERT INTO seen (id, action, seen_at, outcome, expires_at) \
         VALUES ($1, 'enter', now(), 'test', now() + interval '1 hour')",
    )
    .bind(&live_id)
    .execute(store.pool())
    .await
    .expect("insert live row");

    assert!(
        row_exists(&store, &expired_id).await,
        "expired row inserted"
    );
    assert!(row_exists(&store, &live_id).await, "live row inserted");

    let deleted = store.gc_expired().await.expect("gc runs");
    assert!(
        deleted >= 1,
        "GC reports at least the one expired row deleted (got {deleted})"
    );

    assert!(
        !row_exists(&store, &expired_id).await,
        "expired row physically deleted by GC"
    );
    assert!(
        row_exists(&store, &live_id).await,
        "live row survives the GC"
    );

    // Clean up the live row so repeat runs stay tidy.
    sqlx::query("DELETE FROM seen WHERE id = $1")
        .bind(&live_id)
        .execute(store.pool())
        .await
        .expect("cleanup live row");
}
