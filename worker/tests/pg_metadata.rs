//! Cross-backend conformance for the account metadata index — the Postgres
//! half of the parity gate.
//!
//! Drives `trade_control_core::account::metadata_conformance::run_all` (the
//! exact same assertions `core` runs against `MemMetadataStore`) against a live
//! `PgMetadataStore`. If the Mem half passes and this fails (or vice versa), the
//! two `MetadataStore` backends have drifted and it's caught here.
//!
//! Connection comes from `TEST_DATABASE_URL`, defaulting to the local dev DB.
//! Every account name is namespaced by a unique `tag`, so it is safe to run
//! repeatedly against the shared, persistent dev database.

use chrono::Utc;
use trade_control_core::account::metadata_conformance;
use trade_control_worker::{PgMetadataStore, PgStateStore};

fn test_db_url() -> String {
    std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgresql://candle_cache:candle_cache@localhost:5432/trade_control_dev".to_string()
    })
}

#[tokio::test]
async fn metadata_conformance_against_postgres() {
    let state = PgStateStore::connect(&test_db_url())
        .await
        .expect("connect to test db");
    state.migrate().await.expect("run migrations");
    let store = PgMetadataStore::from_state_store(&state);

    // Unique per run so repeat runs on the persistent dev db never collide.
    let tag = format!("pgmeta-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    metadata_conformance::run_all(&store, &tag).await;
}
