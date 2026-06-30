//! Postgres-backed [`MetadataStore`] — the native replacement for the Cloudflare
//! KV account index (`src/accounts/kv_metadata.rs`, one `accounts:index` blob).
//!
//! One row per named account in the `accounts` table (`migrations/0002_accounts.sql`).
//! Typed columns rather than a single jsonb body so dispatch can query by broker
//! and `account list` is a plain ordered `SELECT`. The `broker` / `kind` columns
//! store the lowercase serde form of [`BrokerKind`] / [`AccountKind`]; `caps` is
//! the serde shape of [`AccountCaps`]. Encoding goes through serde (not a hand
//! match) so the column form can't drift from the wire form.
//!
//! Behaviour mirrors [`KvMetadataStore`] exactly (so the existing `account`
//! admin/CLI surface is byte-for-byte unchanged): `list` is name-ascending,
//! `add` rejects a duplicate name, `remove` errors on a missing name.

use sqlx::PgPool;
use sqlx::Row;
use trade_control_core::account::{AccountCaps, AccountMetadata, MetadataError, MetadataStore};
use trade_control_core::intent::BrokerKind;

use crate::PgStateStore;

/// A [`MetadataStore`] backed by the same Postgres pool as [`PgStateStore`].
/// Cheap to clone (the pool is an `Arc` internally).
#[derive(Clone)]
pub struct PgMetadataStore {
    pool: PgPool,
}

impl PgMetadataStore {
    /// Build a metadata store sharing an existing pool — the native runtime
    /// constructs this alongside the [`PgStateStore`] from one connection pool.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Build a metadata store from a [`PgStateStore`], reusing its pool.
    pub fn from_state_store(store: &PgStateStore) -> Self {
        Self {
            pool: store.pool().clone(),
        }
    }
}

/// Map any sqlx error into the trait's opaque backend variant.
fn backend(e: impl std::fmt::Display) -> MetadataError {
    MetadataError::Backend(e.to_string())
}

/// The lowercase serde string for a [`BrokerKind`] (`oanda` | `tradenation`).
/// Goes through serde so it can't drift from the wire form.
fn broker_to_str(broker: BrokerKind) -> String {
    serde_json::to_value(broker)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "oanda".to_string())
}

/// The serde JSON for an [`AccountCaps`] (`{}` when default). Stored in the
/// `caps` jsonb column.
fn caps_to_json(caps: &AccountCaps) -> serde_json::Value {
    serde_json::to_value(caps).unwrap_or_else(|_| serde_json::json!({}))
}

/// Reassemble an [`AccountMetadata`] from its columns. `broker` / `kind` are
/// parsed back through serde from the stored lowercase strings; `caps` from the
/// jsonb. A row that fails to decode is a backend error (corrupt write), not a
/// silent default.
fn row_to_metadata(row: &sqlx::postgres::PgRow) -> Result<AccountMetadata, MetadataError> {
    let name: String = row.try_get("name").map_err(backend)?;
    let broker_str: String = row.try_get("broker").map_err(backend)?;
    let kind_str: String = row.try_get("kind").map_err(backend)?;
    let oanda_account_id: Option<String> = row.try_get("oanda_account_id").map_err(backend)?;
    let caps_json: serde_json::Value = row.try_get("caps").map_err(backend)?;

    let broker = serde_json::from_value(serde_json::Value::String(broker_str.clone()))
        .map_err(|e| MetadataError::Backend(format!("decode broker '{broker_str}': {e}")))?;
    let kind = serde_json::from_value(serde_json::Value::String(kind_str.clone()))
        .map_err(|e| MetadataError::Backend(format!("decode kind '{kind_str}': {e}")))?;
    let caps: AccountCaps = serde_json::from_value(caps_json)
        .map_err(|e| MetadataError::Backend(format!("decode caps: {e}")))?;

    Ok(AccountMetadata {
        name,
        broker,
        kind,
        caps,
        oanda_account_id,
    })
}

impl MetadataStore for PgMetadataStore {
    async fn list(&self) -> Result<Vec<AccountMetadata>, MetadataError> {
        let rows = sqlx::query(
            "SELECT name, broker, kind, oanda_account_id, caps FROM accounts ORDER BY name ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(row_to_metadata).collect()
    }

    async fn get(&self, name: &str) -> Result<AccountMetadata, MetadataError> {
        let row = sqlx::query(
            "SELECT name, broker, kind, oanda_account_id, caps FROM accounts WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        match row {
            Some(r) => row_to_metadata(&r),
            None => Err(MetadataError::NotFound(name.to_owned())),
        }
    }

    async fn add(&self, metadata: AccountMetadata) -> Result<(), MetadataError> {
        // INSERT, mapping a unique-violation on the `name` PK to AlreadyExists so
        // the caller sees the same error the KV store raised (rather than a raw
        // backend string). No read-modify-write race window — the DB enforces it.
        let result = sqlx::query(
            "INSERT INTO accounts (name, broker, kind, oanda_account_id, caps) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&metadata.name)
        .bind(broker_to_str(metadata.broker))
        .bind(kind_to_str(metadata.kind))
        .bind(&metadata.oanda_account_id)
        .bind(caps_to_json(&metadata.caps))
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                Err(MetadataError::AlreadyExists(metadata.name))
            }
            Err(e) => Err(backend(e)),
        }
    }

    async fn remove(&self, name: &str) -> Result<(), MetadataError> {
        let result = sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        if result.rows_affected() == 0 {
            return Err(MetadataError::NotFound(name.to_owned()));
        }
        Ok(())
    }
}

/// The lowercase serde string for an [`trade_control_core::account::AccountKind`]
/// (`demo` | `live`). Goes through serde so it can't drift from the wire form.
fn kind_to_str(kind: trade_control_core::account::AccountKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "demo".to_string())
}
