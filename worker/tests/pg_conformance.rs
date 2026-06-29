//! Cross-backend conformance: the **Postgres** half of the KV→VM parity gate.
//!
//! This drives `trade_control_core::state::conformance::run_all` — the exact
//! same assertions `core` runs against `MemStateStore` — against a live
//! `PgStateStore`. If `core`'s `conformance_against_memstore` passes but this
//! fails (or vice versa), the two `StateStore` backends have diverged and the
//! discrepancy is caught here, before it can reach a live trade.
//!
//! Connection comes from `TEST_DATABASE_URL`, defaulting to the local dev DB.
//! Every id the harness writes is namespaced by a unique `tag`, so it is safe
//! to run repeatedly against the shared, persistent dev database.

use chrono::Utc;
use trade_control_core::state::conformance;
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
async fn conformance_against_postgres() {
    let store = store().await;
    // Unique per run so repeat runs on the persistent dev db never collide.
    let tag = format!("pg-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    conformance::run_all(&store, &tag).await;
}
