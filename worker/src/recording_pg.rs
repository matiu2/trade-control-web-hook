//! The native (Postgres) recording sink — the VM replacement for the wasm
//! worker's two R2 prefixes:
//!   * `req/`   → the `request_records` table ([`record_request`]).
//!   * `ticks/` → the `tick_bundles` table ([`record_tick`]).
//!
//! Both runtimes build the *same* pure record type
//! ([`trade_control_core::recording::RequestRecord`] /
//! [`trade_control_core::tick_bundle::TickBundle`]); here we INSERT it as a
//! `jsonb` body plus the extracted correlation columns the R2 key encoded, so
//! the same date-range + trade-keyed queries are plain indexed SELECTs.
//!
//! Both functions return `Result`, but recording is **fail-soft**: the call
//! sites log + swallow any error. A recording failure must never fail a
//! request or a tick.

use chrono::{DateTime, Utc};
use trade_control_core::recording::RequestRecord;
use trade_control_core::tick_bundle::TickBundle;

/// Insert one webhook [`RequestRecord`] into `request_records`.
///
/// `ts` is the record's RFC3339 string parsed to a `timestamptz`; if the parse
/// fails (a malformed `ts` should never happen — it's minted from `Utc::now`)
/// we fall back to wall-clock `now()` so the row still lands with a sane
/// timestamp. `body` is the whole record as `jsonb`.
pub async fn record_request(
    pool: &sqlx::PgPool,
    record: &RequestRecord,
) -> Result<(), sqlx::Error> {
    let ts: DateTime<Utc> = match DateTime::parse_from_rfc3339(&record.ts) {
        Ok(dt) => dt.with_timezone(&Utc),
        Err(err) => {
            tracing::warn!(
                "recording: RequestRecord.ts {:?} not RFC3339 ({err}) — falling back to now()",
                record.ts
            );
            Utc::now()
        }
    };
    let body = serde_json::to_value(record).map_err(|e| {
        // Wrap a serde error into sqlx's error type so the signature stays a
        // single `sqlx::Error` the call site already log+swallows.
        sqlx::Error::Encode(Box::new(e))
    })?;

    sqlx::query(
        "INSERT INTO request_records \
         (ts, request_id, intent_id, trade_id, status, outcome, body) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(ts)
    .bind(&record.request_id)
    .bind(record.intent_id.as_deref())
    .bind(record.trade_id.as_deref())
    .bind(record.status as i32)
    .bind(&record.outcome)
    .bind(body)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Read every [`RequestRecord`] for one trade, oldest first — the read side of
/// [`record_request`], used by the `plan timeline` reconstruction.
///
/// Matches on the extracted `trade_id` column (indexed) and returns the full
/// records in `ts` order so the caller can render the event timeline for that
/// trade (enters, vetoes, preps and their per-request `logs[]`). Each row's
/// `body jsonb` is deserialized back into the same [`RequestRecord`] the worker
/// wrote — no shape drift, because both sides use the `core` type.
///
/// A row whose `body` fails to deserialize is skipped with a `warn!` rather
/// than failing the whole timeline: one corrupt record must not hide the rest.
pub async fn request_records_for_trade(
    pool: &sqlx::PgPool,
    trade_id: &str,
) -> Result<Vec<RequestRecord>, sqlx::Error> {
    let rows: Vec<(serde_json::Value,)> =
        sqlx::query_as("SELECT body FROM request_records WHERE trade_id = $1 ORDER BY ts, id")
            .bind(trade_id)
            .fetch_all(pool)
            .await?;

    let records = rows
        .into_iter()
        .filter_map(
            |(body,)| match serde_json::from_value::<RequestRecord>(body) {
                Ok(rec) => Some(rec),
                Err(err) => {
                    tracing::warn!("timeline: skipping request_records row for {trade_id}: {err}");
                    None
                }
            },
        )
        .collect();
    Ok(records)
}

/// Read every [`TickBundle`] for one trade, oldest first — the read side of
/// [`record_tick`], used by the `plan timeline` reconstruction.
///
/// A trade is one `correlation_id` (== the plan's `trade_id`), so this is the
/// engine-side analogue of [`request_records_for_trade`]: every cron tick that
/// evaluated this trade's plan, in `tick_ts` order. Together the two streams
/// cover a trade's whole life — inbound alerts and engine ticks. Bundle bodies
/// round-trip through the same `core` [`TickBundle`], so no shape drift.
///
/// A row whose `body` fails to deserialize is skipped with a `warn!` rather
/// than failing the whole timeline, matching [`request_records_for_trade`].
pub async fn tick_bundles_for_trade(
    pool: &sqlx::PgPool,
    trade_id: &str,
) -> Result<Vec<TickBundle>, sqlx::Error> {
    let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT body FROM tick_bundles WHERE correlation_id = $1 ORDER BY tick_ts, id",
    )
    .bind(trade_id)
    .fetch_all(pool)
    .await?;

    let bundles = rows
        .into_iter()
        .filter_map(|(body,)| match serde_json::from_value::<TickBundle>(body) {
            Ok(bundle) => Some(bundle),
            Err(err) => {
                tracing::warn!("timeline: skipping tick_bundles row for {trade_id}: {err}");
                None
            }
        })
        .collect();
    Ok(bundles)
}

/// Insert one cron-engine [`TickBundle`] into `tick_bundles`. `tick_ts` is
/// already a `DateTime<Utc>`; `body` is the whole bundle as `jsonb`.
pub async fn record_tick(pool: &sqlx::PgPool, bundle: &TickBundle) -> Result<(), sqlx::Error> {
    let body = serde_json::to_value(bundle).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

    sqlx::query(
        "INSERT INTO tick_bundles \
         (tick_ts, correlation_id, account, request_id, schema_version, body) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(bundle.tick_ts)
    .bind(&bundle.correlation_id)
    .bind(bundle.account.as_deref())
    .bind(&bundle.request_id)
    .bind(bundle.schema_version as i32)
    .bind(body)
    .execute(pool)
    .await
    .map(|_| ())
}
