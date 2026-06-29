//! Integration test for the native recording sink (`recording_pg`).
//!
//! Inserts a `RequestRecord` via `record_request` and a `TickBundle` via
//! `record_tick`, then SELECTs each back and asserts the extracted correlation
//! columns plus the `jsonb` body round-trips (the exact serde shape the wasm
//! worker would have written to R2).
//!
//! Connection comes from `TEST_DATABASE_URL`, defaulting to the local dev DB
//! (the same one `pg_gc.rs` etc. use). Unique ids keep concurrent/repeat runs
//! from colliding on the shared dev db.

use chrono::Utc;
use trade_control_core::recording::RequestRecord;
use trade_control_core::tick_bundle::TickBundle;
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
async fn record_request_inserts_and_round_trips() {
    let store = store().await;
    let nonce = Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let request_id = format!("test-req-{nonce}");
    let intent_id = format!("ihs-eur-usd-{nonce}");
    let trade_id = format!("ihs-eur-usd-trade-{nonce}");

    let record = RequestRecord {
        ts: "2026-06-15T07:51:39.123Z".to_string(),
        request_id: request_id.clone(),
        method: "POST".to_string(),
        path: "/".to_string(),
        headers: vec![("content-type".to_string(), "text/plain".to_string())],
        body: "action: enter\nid: x\n".to_string(),
        intent_id: Some(intent_id.clone()),
        trade_id: Some(trade_id.clone()),
        status: 200,
        outcome: "ok".to_string(),
        logs: vec![],
    };

    trade_control_worker::recording_pg::record_request(store.pool(), &record)
        .await
        .expect("insert request record");

    // SELECT the row back by request_id and assert the columns + jsonb body.
    let row: (
        String,
        Option<String>,
        Option<String>,
        i32,
        String,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT request_id, intent_id, trade_id, status, outcome, body \
             FROM request_records WHERE request_id = $1",
    )
    .bind(&request_id)
    .fetch_one(store.pool())
    .await
    .expect("select request record");

    assert_eq!(row.0, request_id);
    assert_eq!(row.1.as_deref(), Some(intent_id.as_str()));
    assert_eq!(row.2.as_deref(), Some(trade_id.as_str()));
    assert_eq!(row.3, 200);
    assert_eq!(row.4, "ok");

    // The jsonb body is the verbatim serde of the RequestRecord.
    let expected = serde_json::to_value(&record).expect("serialize record");
    assert_eq!(row.5, expected, "body jsonb round-trips the RequestRecord");

    // Clean up.
    sqlx::query("DELETE FROM request_records WHERE request_id = $1")
        .bind(&request_id)
        .execute(store.pool())
        .await
        .expect("cleanup request record");
}

/// A hand-authored bundle JSON (same fixture shape as `core::tick_bundle`'s
/// test), parameterised with a unique correlation_id so repeat runs don't
/// collide. Built from JSON so it survives `Intent` gaining fields.
fn sample_bundle(correlation_id: &str) -> TickBundle {
    let json = format!(
        r#"{{
      "schema_version": 1,
      "tick_ts": "2026-06-17T20:00:00Z",
      "correlation_id": "{correlation_id}",
      "account": "experimental",
      "request_id": "{correlation_id}@2026-06-17T20:00:00Z",
      "plan": {{
        "trade_id": "{correlation_id}",
        "instrument": "Copper",
        "direction": "short",
        "granularity": "h1",
        "pip_size": 0.1,
        "rules": [],
        "shadow": false
      }},
      "prior_state": {{
        "watermark": "2026-06-17T19:00:00Z",
        "phase": "await_entry",
        "fired": [],
        "last_close": {{}},
        "break_close_at": null,
        "retest_seen_at": null,
        "mw": null,
        "expires_at": "2026-06-18T20:00:00Z"
      }},
      "new_candles": [],
      "detector_window": [],
      "now": "2026-06-17T20:00:00Z",
      "expires_at": "2026-06-18T20:00:00Z",
      "eval": {{
        "fired": [],
        "new_state": {{
          "watermark": "2026-06-17T19:00:00Z",
          "phase": "await_entry",
          "fired": [],
          "last_close": {{}},
          "break_close_at": null,
          "retest_seen_at": null,
          "mw": null,
          "expires_at": "2026-06-18T20:00:00Z"
        }},
        "done": false
      }},
      "shadow": false,
      "dispatch_outcomes": [],
      "kv": {{
        "key": "plan-state:experimental:{correlation_id}",
        "before": null,
        "after": null,
        "cleared_plan": false,
        "success": true,
        "error": null
      }}
    }}"#
    );
    serde_json::from_str(&json).expect("sample bundle parses")
}

#[tokio::test]
async fn record_tick_inserts_and_round_trips() {
    let store = store().await;
    let nonce = Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let correlation_id = format!("hs-copper-{nonce}");
    let bundle = sample_bundle(&correlation_id);

    trade_control_worker::recording_pg::record_tick(store.pool(), &bundle)
        .await
        .expect("insert tick bundle");

    let row: (String, Option<String>, String, i32, serde_json::Value) = sqlx::query_as(
        "SELECT correlation_id, account, request_id, schema_version, body \
         FROM tick_bundles WHERE correlation_id = $1",
    )
    .bind(&correlation_id)
    .fetch_one(store.pool())
    .await
    .expect("select tick bundle");

    assert_eq!(row.0, correlation_id);
    assert_eq!(row.1.as_deref(), Some("experimental"));
    assert_eq!(row.2, format!("{correlation_id}@2026-06-17T20:00:00Z"));
    assert_eq!(row.3, 1);

    let expected = serde_json::to_value(&bundle).expect("serialize bundle");
    assert_eq!(row.4, expected, "body jsonb round-trips the TickBundle");

    sqlx::query("DELETE FROM tick_bundles WHERE correlation_id = $1")
        .bind(&correlation_id)
        .execute(store.pool())
        .await
        .expect("cleanup tick bundle");
}
