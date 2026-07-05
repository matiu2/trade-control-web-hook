//! End-to-end exercise of the **native edge** decision path against real
//! Postgres, with **no live broker**.
//!
//! ## Level tested, and why
//!
//! The native HTTP edge's `dispatch_inner` / `dispatch_control` (in
//! `worker/src/http.rs`) are private — the axum handler ships a body to a local
//! dispatcher thread that owns a full `AppState` (a `PgStateStore` + a
//! `PgMetadataStore` + `Secrets` + a per-account broker factory). Building that
//! whole stack — and stubbing the broker factory it needs for `Enter`/`Close` —
//! is exactly the "awkward, needs a live broker" case the task says to avoid.
//!
//! So this test reproduces the native edge's **broker-free decision sequence**
//! by calling the *same shared functions* `dispatch_inner` calls, in the same
//! order, against a real `PgStateStore`:
//!
//! 1. `trade_control_core::incoming::parse_and_verify` — the signature/parse edge.
//! 2. `store.is_seen` — the replay-protection 409 (shared `StateStore`).
//! 3. the shared control handlers (`handle_status` / `handle_prep` /
//!    `handle_pause`) — which themselves call `record_seen` (the control-action
//!    seen-write semantics).
//! 4. `worker::recording_pg::record_request` — the native request-records insert.
//!
//! That is precisely the path `dispatch_inner` runs for a control action
//! (`http.rs` lines ~195-298) minus the `Send` channel plumbing and the broker
//! branch. Every decision and every Postgres write under test here is the
//! shared, native-edge code; only the axum transport (which is a thin `Send`
//! shim with no decision logic) is not exercised. Broker actions (`Enter` /
//! `Close`) are intentionally out of scope — they need a live broker and are
//! manual/demo-tested, per the task.
//!
//! Postgres comes from `TEST_DATABASE_URL`, defaulting to the local dev DB (same
//! as the other `pg_*.rs` tests). Unique ids keep concurrent/repeat runs from
//! colliding on the shared dev db.

use chrono::{DateTime, Utc};
use trade_control_core::dispatch::{handle_pause, handle_prep, handle_status};
use trade_control_core::incoming::{
    IncomingDisposition, Verified, parse_and_verify, signed_pairs_from_text,
};
use trade_control_core::recording::{RequestRecord, ids_from_body, mint_request_id};
use trade_control_core::sig;
use trade_control_core::state::StateStore;
use trade_control_worker::PgStateStore;

/// Deterministic 32-byte test signing key (hex), decoded to the raw bytes the
/// native `AppState.signing_key` stores. Mirrors the worker's own handler-test
/// key so the wire form is identical.
const TEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

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

fn key() -> Vec<u8> {
    sig::parse_key_hex(TEST_KEY_HEX)
        .expect("valid hex key")
        .to_vec()
}

/// Sign a body the *exact* way the CLI does: line-scan the un-signed YAML into
/// `(key, value)` pairs, HMAC them, then append the `sig:` line. The worker's
/// `parse_and_verify` reconstructs the identical pair list from the same text,
/// so a body produced here verifies on the native edge.
fn sign_body(lines: &[(&str, String)]) -> String {
    let mut body = String::new();
    for (k, v) in lines {
        body.push_str(&format!("{k}: {v}\n"));
    }
    let pairs = signed_pairs_from_text(&body).expect("scan pairs");
    let sig = sig::sign(&key(), &pairs).expect("sign");
    format!("{body}sig: \"{sig}\"\n")
}

/// A far-future `not_after` so the intent never declines as expired.
fn not_after() -> String {
    "2099-01-01T00:00:00Z".to_string()
}

/// A control shell. `Shell` requires `close`/`high`/`low`/`time`; the CLI's
/// `shell_for_control` emits `close/high/low = 0` plus a `time` close to `now`
/// (the freshness check rejects a >24h-old `time`). Mirror that exactly.
fn shell_lines(now: DateTime<Utc>) -> Vec<(&'static str, String)> {
    vec![
        ("close", "0".to_string()),
        ("high", "0".to_string()),
        ("low", "0".to_string()),
        ("time", format!("\"{}\"", now.to_rfc3339())),
    ]
}

/// Mirror the native edge's parse → is_seen sequence (`dispatch_inner`), and run
/// the control body to completion through the shared handler the edge would
/// route it to. Returns the resulting `(verified, was_replay)` so a caller can
/// assert the replay branch directly. The shared control handlers each call
/// `record_seen`, so a second run of the same body 409s here.
async fn verify_and_check_replay(
    store: &PgStateStore,
    body: &str,
    now: DateTime<Utc>,
) -> Result<Verified, ReplayOr400> {
    let verified = parse_and_verify(body, &key(), now).map_err(|err| match err.disposition() {
        IncomingDisposition::Rejected => ReplayOr400::Rejected,
        _ => ReplayOr400::Declined,
    })?;
    // The native edge's replay check (single-shot control actions take the plain
    // 409 path — `is_multishot_enter` is false for every control action).
    if store.is_seen(&verified.intent.id).await.expect("is_seen") {
        return Err(ReplayOr400::Replay);
    }
    Ok(verified)
}

#[derive(Debug, PartialEq)]
enum ReplayOr400 {
    Rejected,
    Declined,
    Replay,
}

// ---------------------------------------------------------------------------
// Case 1 — a malformed / unsigned body is rejected (400 "rejected").
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_body_is_rejected() {
    let now = Utc::now();
    // Not signed YAML at all — the parse/verify edge must reject (the native
    // edge maps `IncomingDisposition::Rejected` → 400 "rejected").
    let err = parse_and_verify("this is not a signed intent", &key(), now).unwrap_err();
    assert_eq!(
        err.disposition(),
        IncomingDisposition::Rejected,
        "a malformed body must map to the 400 `rejected` branch",
    );

    // A well-formed body signed under the WRONG key is likewise rejected.
    let wrong_key =
        sig::parse_key_hex("ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100")
            .unwrap();
    let mut lines = shell_lines(now);
    lines.extend([
        ("v", "1".to_string()),
        ("id", "e2e-wrongkey".to_string()),
        ("action", "status".to_string()),
        ("instrument", "EUR_USD".to_string()),
        ("not_after", format!("\"{}\"", not_after())),
    ]);
    // Build the body but sign with the wrong key.
    let mut body = String::new();
    for (k, v) in &lines {
        body.push_str(&format!("{k}: {v}\n"));
    }
    let pairs = signed_pairs_from_text(&body).unwrap();
    let bad_sig = sig::sign(&wrong_key, &pairs).unwrap();
    let body = format!("{body}sig: \"{bad_sig}\"\n");

    let err = parse_and_verify(&body, &key(), now).unwrap_err();
    assert_eq!(
        err.disposition(),
        IncomingDisposition::Rejected,
        "a body signed under the wrong key must be rejected",
    );
}

// ---------------------------------------------------------------------------
// Case 2 — a valid signed `status` control action: verifies, dispatches to the
// shared handler, returns 200 + the status YAML.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_action_verifies_and_returns_snapshot() {
    let store = store().await;
    let now = Utc::now();
    let id = format!("e2e-status-{}", now.timestamp_nanos_opt().unwrap_or(0));

    let mut lines = shell_lines(now);
    lines.extend([
        ("v", "1".to_string()),
        ("id", id.clone()),
        ("action", "status".to_string()),
        ("instrument", "EUR_USD".to_string()),
        ("not_after", format!("\"{}\"", not_after())),
    ]);
    let body = sign_body(&lines);

    let verified = verify_and_check_replay(&store, &body, now)
        .await
        .expect("status verifies and is not a replay");

    let result = handle_status(&store, &verified, now).await;
    assert_eq!(result.status, 200, "status returns 200");
    // The body is the YAML snapshot of the KV state — `Snapshot` always
    // serialises its `recent_seen` field, so its presence proves we got the
    // snapshot back (and that the handler ran the shared `store.snapshot()`).
    assert!(
        result.body.contains("recent_seen"),
        "status body is the KV snapshot (has recent_seen): {}",
        result.body
    );
}

// ---------------------------------------------------------------------------
// Case 3 — a valid signed `prep` writes state to Postgres; read it back.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prep_action_writes_state_to_postgres() {
    let store = store().await;
    let now = Utc::now();
    let nonce = now.timestamp_nanos_opt().unwrap_or(0);
    let id = format!("e2e-prep-{nonce}");
    // Unique instrument so the prep row can't collide with another run.
    let instrument = format!("E2E_PREP_{nonce}");
    let step = "break-and-close";
    let account = format!("e2e-acct-{nonce}");

    let mut lines = shell_lines(now);
    lines.extend([
        ("v", "1".to_string()),
        ("id", id.clone()),
        ("action", "prep".to_string()),
        ("instrument", instrument.clone()),
        ("account", account.clone()),
        ("step", step.to_string()),
        ("ttl_hours", "12".to_string()),
        ("not_after", format!("\"{}\"", not_after())),
    ]);
    let body = sign_body(&lines);

    // Pre-condition: the prep is not set yet.
    assert!(
        store
            .get_prep(Some(&account), &instrument, step)
            .await
            .unwrap()
            .is_none(),
        "prep absent before dispatch",
    );

    let verified = verify_and_check_replay(&store, &body, now)
        .await
        .expect("prep verifies");
    let result = handle_prep(&store, &verified, now).await;
    assert_eq!(result.status, 200, "prep returns 200: {}", result.body);

    // The state landed in Postgres — read it back via the PgStateStore.
    let set_at = store
        .get_prep(Some(&account), &instrument, step)
        .await
        .unwrap();
    assert!(
        set_at.is_some(),
        "prep state must be persisted to Postgres after a successful prep dispatch",
    );

    // Clean up the prep + the seen row this dispatch wrote.
    store
        .clear_prep(Some(&account), &instrument, step)
        .await
        .unwrap();
    store.forget_seen(&id).await.unwrap();
}

// ---------------------------------------------------------------------------
// Case 4 — replay protection: a control action marks seen; the same id re-fires
// and is caught by the 409 "replay" branch.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replayed_control_action_is_caught_by_seen_index() {
    let store = store().await;
    let now = Utc::now();
    let nonce = now.timestamp_nanos_opt().unwrap_or(0);
    let id = format!("e2e-replay-{nonce}");
    let trade_id = format!("e2e-trade-{nonce}");
    let blackout_id = "news-window";
    let instrument = format!("E2E_PAUSE_{nonce}");

    let mut lines = shell_lines(now);
    lines.extend([
        ("v", "1".to_string()),
        ("id", id.clone()),
        ("action", "pause".to_string()),
        ("instrument", instrument.clone()),
        ("trade_id", trade_id.clone()),
        ("blackout_id", blackout_id.to_string()),
        ("not_after", format!("\"{}\"", not_after())),
    ]);
    let body = sign_body(&lines);

    // First fire: verifies, not yet seen → dispatch → handler marks seen.
    let verified = verify_and_check_replay(&store, &body, now)
        .await
        .expect("first pause fire verifies and is not a replay");
    let result = handle_pause(&store, &verified, now).await;
    assert_eq!(
        result.status, 200,
        "first pause returns 200: {}",
        result.body
    );

    // The pause landed (sanity — handler did its state write).
    assert!(
        !store
            .list_pauses_for_trade(&trade_id)
            .await
            .unwrap()
            .is_empty(),
        "pause must be set after the first fire",
    );

    // Second fire of the SAME body/id: the native edge's `is_seen` check now
    // returns true → 409 "replay" (control actions take the plain replay path;
    // `is_multishot_enter` is false for `pause`).
    let replay = verify_and_check_replay(&store, &body, now).await;
    assert!(
        matches!(replay, Err(ReplayOr400::Replay)),
        "a re-fired control id must be caught by the seen index (409 replay), got {:?}",
        replay.map(|_| "verified-not-replay"),
    );

    // Clean up the pause + seen row.
    store.clear_pause(&trade_id, blackout_id).await.unwrap();
    store.forget_seen(&id).await.unwrap();
}

// ---------------------------------------------------------------------------
// Case 5 — the native request recorder inserts a `request_records` row.
//
// The native edge's `record_request` spawns the insert on a `LocalSet`
// (fire-and-forget). Rather than reproduce that timing-sensitive spawn (and
// risk a flaky test), this calls the same underlying sink — `recording_pg::
// record_request` — directly with the `RequestRecord` the edge builds (via the
// same `ids_from_body` / `mint_request_id` helpers), then reads the row back.
// That deterministically proves the recording wiring the edge depends on; the
// spawn-vs-await detail is the only difference and carries no decision logic.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_record_is_inserted_for_a_dispatched_request() {
    let store = store().await;
    let now = Utc::now();
    let nonce = now.timestamp_nanos_opt().unwrap_or(0);
    let id = format!("e2e-rec-{nonce}");
    let trade_id = format!("e2e-rec-trade-{nonce}");
    let instrument = format!("E2E_REC_{nonce}");

    let mut lines = shell_lines(now);
    lines.extend([
        ("v", "1".to_string()),
        ("id", id.clone()),
        ("action", "pause".to_string()),
        ("instrument", instrument.clone()),
        ("trade_id", trade_id.clone()),
        ("blackout_id", "news-window".to_string()),
        ("not_after", format!("\"{}\"", not_after())),
    ]);
    let body = sign_body(&lines);

    // Build the record exactly as the native edge's `record_request` does.
    let (intent_id, rec_trade_id) = ids_from_body(&body);
    assert_eq!(
        intent_id.as_deref(),
        Some(id.as_str()),
        "ids_from_body extracts the intent id the edge records",
    );
    assert_eq!(rec_trade_id.as_deref(), Some(trade_id.as_str()));

    let request_id = mint_request_id(&body, &[]);
    let record = RequestRecord {
        ts: now.to_rfc3339(),
        request_id: request_id.clone(),
        method: "POST".to_string(),
        path: "/".to_string(),
        headers: vec![],
        body: body.clone(),
        intent_id,
        trade_id: rec_trade_id,
        status: 200,
        outcome: "ok".to_string(),
        logs: vec![],
    };

    trade_control_worker::recording_pg::record_request(store.pool(), &record)
        .await
        .expect("insert request record");

    // Read it back: the native edge persisted a row for this request.
    let row: (String, Option<String>, Option<String>, i32, String) = sqlx::query_as(
        "SELECT request_id, intent_id, trade_id, status, outcome \
         FROM request_records WHERE request_id = $1",
    )
    .bind(&request_id)
    .fetch_one(store.pool())
    .await
    .expect("select request record");

    assert_eq!(row.0, request_id);
    assert_eq!(row.1.as_deref(), Some(id.as_str()));
    assert_eq!(row.2.as_deref(), Some(trade_id.as_str()));
    assert_eq!(row.3, 200);
    assert_eq!(row.4, "ok");

    // Clean up.
    sqlx::query("DELETE FROM request_records WHERE request_id = $1")
        .bind(&request_id)
        .execute(store.pool())
        .await
        .expect("cleanup request record");
}
