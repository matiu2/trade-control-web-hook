//! Phase 0 smoke test for `PgStateStore` — proves connect → migrate → the
//! `seen` family round-trips against a real Postgres. The full cross-backend
//! conformance harness (Mem vs Pg) lands once more families are ported.
//!
//! Connection comes from `TEST_DATABASE_URL`, defaulting to the local dev DB.
//! Each test runs in its own schema-less transaction-free namespace by using a
//! unique id, so it's safe to run against the shared dev database.

use chrono::Utc;
use trade_control_core::intent::Action;
use trade_control_core::state::StateStore;
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

#[tokio::test]
async fn seen_roundtrip() {
    let store = store().await;
    // Unique id so concurrent/repeat runs don't collide on the shared dev db.
    let id = format!(
        "test-seen-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let now = Utc::now();

    assert!(!store.is_seen(&id).await.unwrap(), "absent id is not seen");

    store
        .mark_seen(&id, Action::Enter, now, "entered", 3600, Some("T-123"))
        .await
        .unwrap();

    assert!(store.is_seen(&id).await.unwrap(), "marked id is seen");

    store.forget_seen(&id).await.unwrap();
    assert!(
        !store.is_seen(&id).await.unwrap(),
        "forgotten id is not seen"
    );
}

#[tokio::test]
async fn seen_expires() {
    let store = store().await;
    let id = format!("test-exp-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let now = Utc::now();

    // ttl 0 → expires_at == seen_at == now; `expires_at > now()` is false.
    store
        .mark_seen(&id, Action::Enter, now, "entered", 0, None)
        .await
        .unwrap();
    assert!(
        !store.is_seen(&id).await.unwrap(),
        "ttl-0 id is already expired"
    );
}
