//! Postgres-backed [`StateStore`] — the native (VM + Postgres) replacement for
//! the Cloudflare KV store (`src/state/kv.rs` in the legacy worker).
//!
//! Design (see `MIGRATION-VM-POSTGRES.md`):
//!  * One typed table per state family — schema in `migrations/0001_state.sql`.
//!  * Listing is a plain `SELECT` — the KV `index:*` JSON-blob RMW hack is gone.
//!  * TTL: control rows carry `expires_at`; reads filter `expires_at > now()`.
//!    Per-trade rows (plans, plan_state, archived, entry_attempt, control_event)
//!    have no expiry column (Bug #15).
//!  * Account scope: `account text` NULL = global. Global-first lookups become
//!    `WHERE (account IS NULL OR account = $1)`.
//!  * Structured bodies are stored as `jsonb` of the exact serde shape the KV
//!    store serialised, so parity is a serialisation identity.
//!
//! This is ported incrementally — one state family at a time — as the native
//! worker comes to need each. Methods not yet ported `todo!()` until their
//! family lands (each guarded by a porting checklist in `TODO.md`).

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use trade_control_core::control_event::ControlEvent;
use trade_control_core::intent::{Action, NoEntryWindow};
use trade_control_core::plan_state::PlanState;
use trade_control_core::state::{
    ArchivedPlan, EntryAttempt, MwState, NewsEntry, PauseEntry, SeenEntry, Snapshot,
    SpreadBlackoutRecord, SpreadBlackoutWindow, StateError, StateStore, StoredPlan,
};
use trade_control_core::trade_plan::TradePlan;

/// Map any sqlx error into the store's opaque [`StateError`]. The trait only
/// carries a `Backend(String)` variant — callers treat all backend failures
/// the same (the KV store does likewise).
fn backend(e: impl std::fmt::Display) -> StateError {
    StateError::Backend(e.to_string())
}

/// A [`StateStore`] backed by a Postgres connection pool. Cheap to clone
/// (the pool is an `Arc` internally), so the native runtime can hand a clone
/// to each task that needs state.
#[derive(Clone)]
pub struct PgStateStore {
    pool: PgPool,
}

impl PgStateStore {
    /// Connect to `database_url` and return a ready store. Does **not** run
    /// migrations — call [`Self::migrate`] once at startup for that.
    pub async fn connect(database_url: &str) -> Result<Self, StateError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await
            .map_err(backend)?;
        Ok(Self { pool })
    }

    /// Build a store from an already-constructed pool (tests share one pool).
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply the bundled migrations. Idempotent — sqlx tracks applied versions
    /// in `_sqlx_migrations`.
    pub async fn migrate(&self) -> Result<(), StateError> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(backend)
    }
}

/// Serialise an [`Action`] to its kebab-case wire string (matches the KV store,
/// which stored the serde form). Infallible for this enum; falls back to
/// `enter` only if serde ever changed shape under us.
fn action_to_str(action: Action) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "enter".to_string())
}

/// Parse an [`Action`] back from its wire string. Unknown strings fall back to
/// `Enter`, matching the KV store's `default_action` for legacy rows.
fn action_from_str(s: &str) -> Action {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or(Action::Enter)
}

// ───────────────────────────── seen (replay) ────────────────────────────────

impl PgStateStore {
    pub(crate) async fn is_seen_impl(&self, id: &str) -> Result<bool, StateError> {
        let row: Option<(bool,)> =
            sqlx::query_as("SELECT true FROM seen WHERE id = $1 AND expires_at > now()")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(backend)?;
        Ok(row.is_some())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn mark_seen_impl(
        &self,
        id: &str,
        action: Action,
        seen_at: DateTime<Utc>,
        outcome: &str,
        ttl_seconds: u64,
        trade_id: Option<&str>,
    ) -> Result<(), StateError> {
        let expires_at = seen_at + chrono::Duration::seconds(ttl_seconds as i64);
        sqlx::query(
            "INSERT INTO seen (id, action, seen_at, outcome, trade_id, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO UPDATE SET
               action = EXCLUDED.action,
               seen_at = EXCLUDED.seen_at,
               outcome = EXCLUDED.outcome,
               trade_id = EXCLUDED.trade_id,
               expires_at = EXCLUDED.expires_at",
        )
        .bind(id)
        .bind(action_to_str(action))
        .bind(seen_at)
        .bind(outcome)
        .bind(trade_id)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    pub(crate) async fn forget_seen_impl(&self, id: &str) -> Result<(), StateError> {
        sqlx::query("DELETE FROM seen WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    /// All live seen rows, newest-recorded first (matches the KV index order
    /// used by `snapshot`). Used by `recent_seen` in the snapshot.
    pub(crate) async fn live_seen(&self) -> Result<Vec<SeenEntry>, StateError> {
        let rows: Vec<SeenRow> = sqlx::query_as(
            "SELECT id, action, seen_at, outcome, trade_id, expires_at
             FROM seen WHERE expires_at > now()
             ORDER BY seen_at DESC NULLS LAST",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        Ok(rows.into_iter().map(SeenRow::into_entry).collect())
    }
}

/// Row shape for the `seen` table. Kept private; converted to the public
/// [`SeenEntry`] on read so the enum/Option mapping lives in one place.
#[derive(sqlx::FromRow)]
struct SeenRow {
    id: String,
    action: String,
    seen_at: Option<DateTime<Utc>>,
    outcome: String,
    trade_id: Option<String>,
    expires_at: DateTime<Utc>,
}

impl SeenRow {
    fn into_entry(self) -> SeenEntry {
        SeenEntry {
            id: self.id,
            action: action_from_str(&self.action),
            seen_at: self.seen_at,
            outcome: self.outcome,
            expires_at: self.expires_at,
            trade_id: self.trade_id,
        }
    }
}
// ─────────────────── cooldown (account+instrument, TTL) ─────────────────────

impl PgStateStore {
    async fn is_cooled_down_impl(
        &self,
        account: Option<&str>,
        instrument: &str,
    ) -> Result<bool, StateError> {
        // Global-first: a global (account IS NULL) cooldown pauses every
        // account; a scoped query also matches it.
        let row: Option<(bool,)> = sqlx::query_as(
            "SELECT true FROM cooldown
             WHERE instrument = $2 AND expires_at > now()
               AND (account IS NULL OR account = $1)
             LIMIT 1",
        )
        .bind(account)
        .bind(instrument)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        Ok(row.is_some())
    }

    async fn set_cooldown_impl(
        &self,
        account: Option<&str>,
        instrument: &str,
        hours: u32,
        now: DateTime<Utc>,
    ) -> Result<(), StateError> {
        let expires_at = now + chrono::Duration::hours(hours as i64);
        // NULL account can't participate in a UNIQUE/ON CONFLICT key (NULLs are
        // distinct), so upsert via delete+insert within the (account,instrument)
        // scope. Matches KV's overwrite semantics.
        sqlx::query(
            "DELETE FROM cooldown WHERE account IS NOT DISTINCT FROM $1 AND instrument = $2",
        )
        .bind(account)
        .bind(instrument)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        sqlx::query(
            "INSERT INTO cooldown (account, instrument, set_at, expires_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(account)
        .bind(instrument)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn clear_cooldown_impl(
        &self,
        account: Option<&str>,
        instrument: &str,
    ) -> Result<bool, StateError> {
        // Scoped clear: only this account's row (or the global row if account
        // is None). `IS NOT DISTINCT FROM` treats NULL = NULL as a match.
        let res = sqlx::query(
            "DELETE FROM cooldown
             WHERE account IS NOT DISTINCT FROM $1 AND instrument = $2
               AND expires_at > now()",
        )
        .bind(account)
        .bind(instrument)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(res.rows_affected() > 0)
    }
}

// ───────────────────── prep (account+instrument+step, TTL) ──────────────────

impl PgStateStore {
    #[allow(clippy::too_many_arguments)]
    async fn set_prep_impl(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
        now: DateTime<Utc>,
        ttl_seconds: u64,
        setter_id: &str,
    ) -> Result<(), StateError> {
        let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
        sqlx::query(
            "DELETE FROM prep
             WHERE account IS NOT DISTINCT FROM $1 AND instrument = $2 AND step = $3",
        )
        .bind(account)
        .bind(instrument)
        .bind(step)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        sqlx::query(
            "INSERT INTO prep (account, instrument, step, set_at, setter_id, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(account)
        .bind(instrument)
        .bind(step)
        .bind(now)
        .bind(setter_id)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn get_prep_impl(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> Result<Option<DateTime<Utc>>, StateError> {
        // Global-first: returns the prep's `set_at` (used by the ordering gate).
        let row: Option<(DateTime<Utc>,)> = sqlx::query_as(
            "SELECT set_at FROM prep
             WHERE instrument = $2 AND step = $3 AND expires_at > now()
               AND (account IS NULL OR account = $1)
             ORDER BY account NULLS LAST
             LIMIT 1",
        )
        .bind(account)
        .bind(instrument)
        .bind(step)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        Ok(row.map(|(set_at,)| set_at))
    }

    async fn clear_prep_impl(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> Result<Option<String>, StateError> {
        // Returns Some(setter_id) if it was active, None if it wasn't set.
        // Scoped to the supplied account.
        let row: Option<(String,)> = sqlx::query_as(
            "DELETE FROM prep
             WHERE account IS NOT DISTINCT FROM $1 AND instrument = $2 AND step = $3
               AND expires_at > now()
             RETURNING setter_id",
        )
        .bind(account)
        .bind(instrument)
        .bind(step)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        Ok(row.map(|(s,)| s))
    }
}

// ──────────────── veto (account+trade_id+instrument+name, TTL) ──────────────

impl PgStateStore {
    async fn set_veto_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
        instrument: &str,
        name: &str,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        // Presence is the signal; we still stamp expires_at for TTL.
        let expires_at = Utc::now() + chrono::Duration::seconds(ttl_seconds as i64);
        sqlx::query(
            "DELETE FROM veto
             WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2
               AND instrument = $3 AND name = $4",
        )
        .bind(account)
        .bind(trade_id)
        .bind(instrument)
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        sqlx::query(
            "INSERT INTO veto (account, trade_id, instrument, name, expires_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(account)
        .bind(trade_id)
        .bind(instrument)
        .bind(name)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn is_vetoed_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
        instrument: &str,
        name: &str,
    ) -> Result<bool, StateError> {
        // Global-first: a Some(account) query matches a global (NULL) veto too.
        // trade_id must match exactly.
        let row: Option<(bool,)> = sqlx::query_as(
            "SELECT true FROM veto
             WHERE trade_id = $2 AND instrument = $3 AND name = $4 AND expires_at > now()
               AND (account IS NULL OR account = $1)
             LIMIT 1",
        )
        .bind(account)
        .bind(trade_id)
        .bind(instrument)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        Ok(row.is_some())
    }

    async fn clear_veto_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
        instrument: &str,
        name: &str,
    ) -> Result<bool, StateError> {
        let res = sqlx::query(
            "DELETE FROM veto
             WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2
               AND instrument = $3 AND name = $4 AND expires_at > now()",
        )
        .bind(account)
        .bind(trade_id)
        .bind(instrument)
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(res.rows_affected() > 0)
    }
}

// ─────────────── prep_block (account+instrument+step, TTL) ──────────────────

impl PgStateStore {
    async fn block_prep_impl(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
        sqlx::query(
            "DELETE FROM prep_block
             WHERE account IS NOT DISTINCT FROM $1 AND instrument = $2 AND step = $3",
        )
        .bind(account)
        .bind(instrument)
        .bind(step)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        sqlx::query(
            "INSERT INTO prep_block (account, instrument, step, set_at, expires_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(account)
        .bind(instrument)
        .bind(step)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn is_prep_blocked_impl(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> Result<bool, StateError> {
        let row: Option<(bool,)> = sqlx::query_as(
            "SELECT true FROM prep_block
             WHERE instrument = $2 AND step = $3 AND expires_at > now()
               AND (account IS NULL OR account = $1)
             LIMIT 1",
        )
        .bind(account)
        .bind(instrument)
        .bind(step)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        Ok(row.is_some())
    }

    async fn clear_prep_block_impl(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> Result<bool, StateError> {
        let res = sqlx::query(
            "DELETE FROM prep_block
             WHERE account IS NOT DISTINCT FROM $1 AND instrument = $2 AND step = $3
               AND expires_at > now()",
        )
        .bind(account)
        .bind(instrument)
        .bind(step)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(res.rows_affected() > 0)
    }
}

// ──────────────────── pause (trade_id+blackout_id, TTL) ─────────────────────

impl PgStateStore {
    async fn set_pause_impl(
        &self,
        trade_id: &str,
        blackout_id: &str,
        reason: Option<&str>,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
        sqlx::query(
            "INSERT INTO pause (trade_id, blackout_id, reason, set_at, expires_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (trade_id, blackout_id) DO UPDATE SET
               reason = EXCLUDED.reason, set_at = EXCLUDED.set_at,
               expires_at = EXCLUDED.expires_at",
        )
        .bind(trade_id)
        .bind(blackout_id)
        .bind(reason)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn list_pauses_for_trade_impl(
        &self,
        trade_id: &str,
    ) -> Result<Vec<PauseEntry>, StateError> {
        let rows: Vec<PauseRow> = sqlx::query_as(
            "SELECT trade_id, blackout_id, reason, set_at, expires_at
             FROM pause WHERE trade_id = $1 AND expires_at > now()",
        )
        .bind(trade_id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        Ok(rows.into_iter().map(PauseRow::into_entry).collect())
    }

    async fn clear_pause_impl(
        &self,
        trade_id: &str,
        blackout_id: &str,
    ) -> Result<bool, StateError> {
        let res = sqlx::query(
            "DELETE FROM pause WHERE trade_id = $1 AND blackout_id = $2 AND expires_at > now()",
        )
        .bind(trade_id)
        .bind(blackout_id)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(res.rows_affected() > 0)
    }
}

#[derive(sqlx::FromRow)]
struct PauseRow {
    trade_id: String,
    blackout_id: String,
    reason: Option<String>,
    set_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

impl PauseRow {
    fn into_entry(self) -> PauseEntry {
        PauseEntry {
            trade_id: self.trade_id,
            blackout_id: self.blackout_id,
            reason: self.reason,
            set_at: self.set_at,
            expires_at: self.expires_at,
        }
    }
}

// ─────────────────── news_window (trade_id+news_id, TTL) ────────────────────

impl PgStateStore {
    async fn set_news_window_impl(
        &self,
        trade_id: &str,
        news_id: &str,
        reason: Option<&str>,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
        sqlx::query(
            "INSERT INTO news_window (trade_id, news_id, reason, set_at, expires_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (trade_id, news_id) DO UPDATE SET
               reason = EXCLUDED.reason, set_at = EXCLUDED.set_at,
               expires_at = EXCLUDED.expires_at",
        )
        .bind(trade_id)
        .bind(news_id)
        .bind(reason)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn list_news_windows_for_trade_impl(
        &self,
        trade_id: &str,
    ) -> Result<Vec<NewsEntry>, StateError> {
        let rows: Vec<NewsRow> = sqlx::query_as(
            "SELECT trade_id, news_id, reason, set_at, expires_at
             FROM news_window WHERE trade_id = $1 AND expires_at > now()",
        )
        .bind(trade_id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        Ok(rows.into_iter().map(NewsRow::into_entry).collect())
    }

    async fn clear_news_window_impl(
        &self,
        trade_id: &str,
        news_id: &str,
    ) -> Result<bool, StateError> {
        let res = sqlx::query(
            "DELETE FROM news_window WHERE trade_id = $1 AND news_id = $2 AND expires_at > now()",
        )
        .bind(trade_id)
        .bind(news_id)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(res.rows_affected() > 0)
    }
}

#[derive(sqlx::FromRow)]
struct NewsRow {
    trade_id: String,
    news_id: String,
    reason: Option<String>,
    set_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

impl NewsRow {
    fn into_entry(self) -> NewsEntry {
        NewsEntry {
            trade_id: self.trade_id,
            news_id: self.news_id,
            reason: self.reason,
            set_at: self.set_at,
            expires_at: self.expires_at,
        }
    }
}

// ───────────── retry_fire (account+trade_id+shell_time dedup, TTL) ──────────

impl PgStateStore {
    async fn is_retry_fire_seen_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
        shell_time: DateTime<Utc>,
    ) -> Result<bool, StateError> {
        let row: Option<(bool,)> = sqlx::query_as(
            "SELECT true FROM retry_fire
             WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2 AND shell_time = $3
               AND expires_at > now()",
        )
        .bind(account)
        .bind(trade_id)
        .bind(shell_time)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        Ok(row.is_some())
    }

    async fn mark_retry_fire_seen_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
        shell_time: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let expires_at = Utc::now() + chrono::Duration::seconds(ttl_seconds as i64);
        sqlx::query(
            "DELETE FROM retry_fire
             WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2 AND shell_time = $3",
        )
        .bind(account)
        .bind(trade_id)
        .bind(shell_time)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        sqlx::query(
            "INSERT INTO retry_fire (account, trade_id, shell_time, expires_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(account)
        .bind(trade_id)
        .bind(shell_time)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
}

// ════════════════════════ jsonb-body families ══════════════════════════════
// These store the whole serde struct as `jsonb` and pull scalar key columns
// out for indexing. Round-trip is a serialisation identity — same wire bytes
// the KV store wrote, so parity is exact.

/// Serialise a value to `serde_json::Value` for a jsonb bind, mapping serde
/// failure into a backend error (these types all derive Serialize, so this is
/// effectively infallible — but we never `.unwrap()`).
fn to_jsonb<T: serde::Serialize>(v: &T) -> Result<serde_json::Value, StateError> {
    serde_json::to_value(v).map_err(backend)
}

/// Deserialise a jsonb `Value` back into a typed body.
fn from_jsonb<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T, StateError> {
    serde_json::from_value(v).map_err(backend)
}

// ───────────────── entry_attempt (account+trade_id+attempt_no) ──────────────

impl PgStateStore {
    async fn record_entry_attempt_impl(&self, attempt: EntryAttempt) -> Result<(), StateError> {
        let body = to_jsonb(&attempt)?;
        sqlx::query(
            "INSERT INTO entry_attempt (account, trade_id, attempt_no, body)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (account, trade_id, attempt_no) DO UPDATE SET body = EXCLUDED.body",
        )
        .bind(attempt.account.as_deref())
        .bind(&attempt.trade_id)
        .bind(attempt.attempt_no as i32)
        .bind(body)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn list_entry_attempts_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Vec<EntryAttempt>, StateError> {
        // Scoped exactly to (account, trade_id) — NOT global-first (matches KV,
        // which keys attempts by the scope the place ran under).
        let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
            "SELECT body FROM entry_attempt
             WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2
             ORDER BY attempt_no ASC",
        )
        .bind(account)
        .bind(trade_id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter().map(|(v,)| from_jsonb(v)).collect()
    }

    async fn list_all_entry_attempts_impl(&self) -> Result<Vec<EntryAttempt>, StateError> {
        let rows: Vec<(serde_json::Value,)> = sqlx::query_as("SELECT body FROM entry_attempt")
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?;
        rows.into_iter().map(|(v,)| from_jsonb(v)).collect()
    }

    async fn delete_entry_attempt_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
        attempt_no: u32,
    ) -> Result<(), StateError> {
        sqlx::query(
            "DELETE FROM entry_attempt
             WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2 AND attempt_no = $3",
        )
        .bind(account)
        .bind(trade_id)
        .bind(attempt_no as i32)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn set_entry_attempt_broker_trade_id_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
        attempt_no: u32,
        broker_trade_id: &str,
    ) -> Result<(), StateError> {
        // Read-modify-write the jsonb body's broker_trade_id field. Idempotent.
        // jsonb_set writes the value as a JSON string.
        sqlx::query(
            "UPDATE entry_attempt
             SET body = jsonb_set(body, '{broker_trade_id}', to_jsonb($4::text), true)
             WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2 AND attempt_no = $3",
        )
        .bind(account)
        .bind(trade_id)
        .bind(attempt_no as i32)
        .bind(broker_trade_id)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
}

// ───────────── spread_blackout_window (singleton marker, TTL) ───────────────

impl PgStateStore {
    async fn set_spread_blackout_window_impl(
        &self,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
        let body = to_jsonb(&SpreadBlackoutWindow {
            opened_at: now,
            expires_at,
        })?;
        sqlx::query(
            "INSERT INTO spread_blackout_window (singleton, body, expires_at)
             VALUES (true, $1, $2)
             ON CONFLICT (singleton) DO UPDATE SET body = EXCLUDED.body,
               expires_at = EXCLUDED.expires_at",
        )
        .bind(body)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn get_spread_blackout_window_impl(
        &self,
    ) -> Result<Option<SpreadBlackoutWindow>, StateError> {
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT body FROM spread_blackout_window WHERE singleton AND expires_at > now()",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.map(|(v,)| from_jsonb(v)).transpose()
    }
}

// ──────────────── blackout_windows (per-instrument, TTL) ────────────────────

impl PgStateStore {
    async fn set_blackout_windows_impl(
        &self,
        instrument: &str,
        windows: &[NoEntryWindow],
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
        let body = to_jsonb(&windows.to_vec())?;
        sqlx::query(
            "INSERT INTO blackout_windows (instrument, windows, updated_at, expires_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (instrument) DO UPDATE SET windows = EXCLUDED.windows,
               updated_at = EXCLUDED.updated_at, expires_at = EXCLUDED.expires_at",
        )
        .bind(instrument)
        .bind(body)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn get_blackout_windows_impl(
        &self,
        instrument: &str,
    ) -> Result<Vec<NoEntryWindow>, StateError> {
        // Fail-open: absent / expired → empty Vec.
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT windows FROM blackout_windows WHERE instrument = $1 AND expires_at > now()",
        )
        .bind(instrument)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        match row {
            Some((v,)) => from_jsonb(v),
            None => Ok(Vec::new()),
        }
    }
}

// ──────────────── spread_blackout_record (per-trade, TTL) ───────────────────

impl PgStateStore {
    async fn upsert_spread_blackout_record_impl(
        &self,
        record: &SpreadBlackoutRecord,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let expires_at = Utc::now() + chrono::Duration::seconds(ttl_seconds as i64);
        let body = to_jsonb(record)?;
        sqlx::query(
            "INSERT INTO spread_blackout_record (trade_id, body, expires_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (trade_id) DO UPDATE SET body = EXCLUDED.body,
               expires_at = EXCLUDED.expires_at",
        )
        .bind(&record.trade_id)
        .bind(body)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn get_spread_blackout_record_impl(
        &self,
        trade_id: &str,
    ) -> Result<Option<SpreadBlackoutRecord>, StateError> {
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT body FROM spread_blackout_record WHERE trade_id = $1 AND expires_at > now()",
        )
        .bind(trade_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.map(|(v,)| from_jsonb(v)).transpose()
    }

    async fn list_all_spread_blackout_records_impl(
        &self,
    ) -> Result<Vec<SpreadBlackoutRecord>, StateError> {
        let rows: Vec<(serde_json::Value,)> =
            sqlx::query_as("SELECT body FROM spread_blackout_record WHERE expires_at > now()")
                .fetch_all(&self.pool)
                .await
                .map_err(backend)?;
        rows.into_iter().map(|(v,)| from_jsonb(v)).collect()
    }

    async fn clear_spread_blackout_record_impl(&self, trade_id: &str) -> Result<(), StateError> {
        sqlx::query("DELETE FROM spread_blackout_record WHERE trade_id = $1")
            .bind(trade_id)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }
}

// ──────────────────── mw_state (account+trade_id, TTL) ──────────────────────

impl PgStateStore {
    async fn get_mw_state_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Option<MwState>, StateError> {
        // Global-first like the veto/prep lookups.
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT body FROM mw_state
             WHERE trade_id = $2 AND expires_at > now()
               AND (account IS NULL OR account = $1)
             ORDER BY account NULLS LAST
             LIMIT 1",
        )
        .bind(account)
        .bind(trade_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.map(|(v,)| from_jsonb(v)).transpose()
    }

    async fn upsert_mw_state_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
        state: &MwState,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let expires_at = Utc::now() + chrono::Duration::seconds(ttl_seconds as i64);
        let body = to_jsonb(state)?;
        sqlx::query("DELETE FROM mw_state WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2")
            .bind(account)
            .bind(trade_id)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        sqlx::query(
            "INSERT INTO mw_state (account, trade_id, body, expires_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(account)
        .bind(trade_id)
        .bind(body)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn clear_mw_state_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        sqlx::query("DELETE FROM mw_state WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2")
            .bind(account)
            .bind(trade_id)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }
}

// ──────────── trade_plan / plan_state (account+trade_id, NO TTL) ────────────

impl PgStateStore {
    async fn put_trade_plan_impl(
        &self,
        account: Option<&str>,
        plan: &TradePlan,
    ) -> Result<(), StateError> {
        let body = to_jsonb(plan)?;
        sqlx::query(
            "DELETE FROM trade_plan WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2",
        )
        .bind(account)
        .bind(&plan.trade_id)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        sqlx::query("INSERT INTO trade_plan (account, trade_id, body) VALUES ($1, $2, $3)")
            .bind(account)
            .bind(&plan.trade_id)
            .bind(body)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn get_trade_plan_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Option<TradePlan>, StateError> {
        // Account-scoped only (no global-first) — a plan key always carries the
        // registering intent's scope.
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT body FROM trade_plan WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2",
        )
        .bind(account)
        .bind(trade_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.map(|(v,)| from_jsonb(v)).transpose()
    }

    async fn list_all_trade_plans_impl(&self) -> Result<Vec<StoredPlan>, StateError> {
        let rows: Vec<(Option<String>, serde_json::Value)> =
            sqlx::query_as("SELECT account, body FROM trade_plan")
                .fetch_all(&self.pool)
                .await
                .map_err(backend)?;
        rows.into_iter()
            .map(|(account, v)| {
                Ok(StoredPlan {
                    account,
                    plan: from_jsonb(v)?,
                })
            })
            .collect()
    }

    async fn clear_trade_plan_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        sqlx::query(
            "DELETE FROM trade_plan WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2",
        )
        .bind(account)
        .bind(trade_id)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn get_plan_state_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Option<PlanState>, StateError> {
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT body FROM plan_state WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2",
        )
        .bind(account)
        .bind(trade_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.map(|(v,)| from_jsonb(v)).transpose()
    }

    async fn put_plan_state_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
        state: &PlanState,
    ) -> Result<(), StateError> {
        let body = to_jsonb(state)?;
        sqlx::query(
            "DELETE FROM plan_state WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2",
        )
        .bind(account)
        .bind(trade_id)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        sqlx::query("INSERT INTO plan_state (account, trade_id, body) VALUES ($1, $2, $3)")
            .bind(account)
            .bind(trade_id)
            .bind(body)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn clear_plan_state_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        sqlx::query(
            "DELETE FROM plan_state WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2",
        )
        .bind(account)
        .bind(trade_id)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
}

// ─────────────── control_event (append-only audit, NO TTL) ──────────────────

impl PgStateStore {
    async fn record_control_event_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
        event: &ControlEvent,
    ) -> Result<(), StateError> {
        // Append-only: a later set of the same control is a distinct event.
        // `seq` (bigserial) orders within (account, trade_id, key_suffix); the
        // PK includes it so re-recording the same key_suffix at the same instant
        // still appends rather than collides. KV keyed on key_suffix alone, so
        // an identical (trade,suffix) overwrote — but key_suffix embeds the
        // set_at epoch, so same-suffix means same instant, where append vs
        // overwrite is observationally identical for the audit reader.
        let body = to_jsonb(event)?;
        sqlx::query(
            "INSERT INTO control_event (account, trade_id, key_suffix, body, set_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(account)
        .bind(trade_id)
        .bind(event.key_suffix())
        .bind(body)
        .bind(event.set_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn list_control_events_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Vec<ControlEvent>, StateError> {
        let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
            "SELECT body FROM control_event
             WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2
             ORDER BY set_at ASC, seq ASC",
        )
        .bind(account)
        .bind(trade_id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter().map(|(v,)| from_jsonb(v)).collect()
    }

    async fn clear_control_events_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        sqlx::query(
            "DELETE FROM control_event WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2",
        )
        .bind(account)
        .bind(trade_id)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
}

// ───────────────── archived_plan (account+trade_id, NO TTL) ─────────────────

impl PgStateStore {
    async fn archive_plan_impl(
        &self,
        account: Option<&str>,
        plan: &TradePlan,
        final_state: &PlanState,
        archived_at: DateTime<Utc>,
    ) -> Result<(), StateError> {
        // ArchivedPlan's `account` field is #[serde(skip)] — the column is the
        // source of truth, recovered on read. Body carries plan/final_state/at.
        let archived = ArchivedPlan {
            account: None, // skipped in serialisation; column holds the truth
            plan: plan.clone(),
            final_state: final_state.clone(),
            archived_at,
        };
        let body = to_jsonb(&archived)?;
        sqlx::query(
            "INSERT INTO archived_plan (account, trade_id, body)
             VALUES ($1, $2, $3)
             ON CONFLICT (account, trade_id) DO UPDATE SET body = EXCLUDED.body",
        )
        .bind(account)
        .bind(&plan.trade_id)
        .bind(body)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn list_all_archived_plans_impl(&self) -> Result<Vec<ArchivedPlan>, StateError> {
        let rows: Vec<(Option<String>, serde_json::Value)> =
            sqlx::query_as("SELECT account, body FROM archived_plan")
                .fetch_all(&self.pool)
                .await
                .map_err(backend)?;
        rows.into_iter()
            .map(|(account, v)| {
                let mut ap: ArchivedPlan = from_jsonb(v)?;
                ap.account = account; // recover the skipped field from the column
                Ok(ap)
            })
            .collect()
    }

    async fn clear_archived_plan_impl(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        sqlx::query(
            "DELETE FROM archived_plan WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2",
        )
        .bind(account)
        .bind(trade_id)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
}

// ───────────────────────────── snapshot ────────────────────────────────────
// A real snapshot via plain SELECTs — the KV store needed a parallel JSON
// index to answer this; Postgres just lists. (MemStateStore returns empties,
// so the conformance harness verifies snapshot against Pg only.)

#[derive(sqlx::FromRow)]
struct CooldownRow {
    account: Option<String>,
    instrument: String,
    set_at: Option<DateTime<Utc>>,
    expires_at: DateTime<Utc>,
}

/// Snapshot row for `prep`. `setter_id` is intentionally not selected — the
/// public [`PrepEntry`] doesn't carry it (it's only used by `clear_prep`'s
/// RETURNING path), so listing it here would be dead.
#[derive(sqlx::FromRow)]
struct PrepRow {
    account: Option<String>,
    instrument: String,
    step: String,
    set_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct VetoRow {
    account: Option<String>,
    trade_id: String,
    instrument: String,
    name: String,
    expires_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct PrepBlockRow {
    account: Option<String>,
    instrument: String,
    step: String,
    expires_at: DateTime<Utc>,
}

impl PgStateStore {
    async fn snapshot_impl(&self) -> Result<Snapshot, StateError> {
        use trade_control_core::state::{CooldownEntry, PrepBlockEntry, PrepEntry, VetoEntry};

        let now = Utc::now();

        let cooldowns: Vec<CooldownRow> = sqlx::query_as(
            "SELECT account, instrument, set_at, expires_at FROM cooldown WHERE expires_at > now()",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;

        let preps: Vec<PrepRow> = sqlx::query_as(
            "SELECT account, instrument, step, set_at, expires_at
             FROM prep WHERE expires_at > now()",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;

        let vetos: Vec<VetoRow> = sqlx::query_as(
            "SELECT account, trade_id, instrument, name, expires_at
             FROM veto WHERE expires_at > now()",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;

        let prep_blocks: Vec<PrepBlockRow> = sqlx::query_as(
            "SELECT account, instrument, step, expires_at FROM prep_block WHERE expires_at > now()",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;

        let pauses: Vec<PauseRow> = sqlx::query_as(
            "SELECT trade_id, blackout_id, reason, set_at, expires_at
             FROM pause WHERE expires_at > now()",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;

        let news_windows: Vec<NewsRow> = sqlx::query_as(
            "SELECT trade_id, news_id, reason, set_at, expires_at
             FROM news_window WHERE expires_at > now()",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;

        Ok(Snapshot {
            now,
            cooldowns: cooldowns
                .into_iter()
                .map(|r| CooldownEntry {
                    instrument: r.instrument,
                    set_at: r.set_at,
                    expires_at: r.expires_at,
                    account: r.account,
                })
                .collect(),
            recent_seen: self.live_seen().await?,
            preps: preps
                .into_iter()
                .map(|r| PrepEntry {
                    instrument: r.instrument,
                    step: r.step,
                    set_at: r.set_at,
                    expires_at: r.expires_at,
                    account: r.account,
                })
                .collect(),
            vetos: vetos
                .into_iter()
                .map(|r| VetoEntry {
                    trade_id: r.trade_id,
                    instrument: r.instrument,
                    name: r.name,
                    expires_at: r.expires_at,
                    account: r.account,
                })
                .collect(),
            pauses: pauses.into_iter().map(PauseRow::into_entry).collect(),
            news_windows: news_windows.into_iter().map(NewsRow::into_entry).collect(),
            prep_blocks: prep_blocks
                .into_iter()
                .map(|r| PrepBlockEntry {
                    instrument: r.instrument,
                    step: r.step,
                    expires_at: r.expires_at,
                    account: r.account,
                })
                .collect(),
            spread_blackouts: self.list_all_spread_blackout_records_impl().await?,
            spread_blackout_window: self.get_spread_blackout_window_impl().await?,
        })
    }
}

// AUTO-PORTED TRAIT IMPL — seen family delegates; rest todo!() until ported.
impl StateStore for PgStateStore {
    async fn is_seen(&self, id: &str) -> Result<bool, StateError> {
        self.is_seen_impl(id).await
    }
    async fn mark_seen(
        &self,
        id: &str,
        action: Action,
        seen_at: DateTime<Utc>,
        outcome: &str,
        ttl_seconds: u64,
        trade_id: Option<&str>,
    ) -> Result<(), StateError> {
        self.mark_seen_impl(id, action, seen_at, outcome, ttl_seconds, trade_id)
            .await
    }
    async fn forget_seen(&self, id: &str) -> Result<(), StateError> {
        self.forget_seen_impl(id).await
    }
    async fn is_cooled_down(
        &self,
        account: Option<&str>,
        instrument: &str,
    ) -> Result<bool, StateError> {
        self.is_cooled_down_impl(account, instrument).await
    }
    async fn set_cooldown(
        &self,
        account: Option<&str>,
        instrument: &str,
        hours: u32,
        now: DateTime<Utc>,
    ) -> Result<(), StateError> {
        self.set_cooldown_impl(account, instrument, hours, now)
            .await
    }
    async fn clear_cooldown(
        &self,
        account: Option<&str>,
        instrument: &str,
    ) -> Result<bool, StateError> {
        self.clear_cooldown_impl(account, instrument).await
    }
    async fn set_prep(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
        now: DateTime<Utc>,
        ttl_seconds: u64,
        setter_id: &str,
    ) -> Result<(), StateError> {
        self.set_prep_impl(account, instrument, step, now, ttl_seconds, setter_id)
            .await
    }
    async fn get_prep(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> Result<Option<DateTime<Utc>>, StateError> {
        self.get_prep_impl(account, instrument, step).await
    }
    async fn clear_prep(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> Result<Option<String>, StateError> {
        self.clear_prep_impl(account, instrument, step).await
    }
    async fn set_veto(
        &self,
        account: Option<&str>,
        trade_id: &str,
        instrument: &str,
        name: &str,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        self.set_veto_impl(account, trade_id, instrument, name, ttl_seconds)
            .await
    }
    async fn is_vetoed(
        &self,
        account: Option<&str>,
        trade_id: &str,
        instrument: &str,
        name: &str,
    ) -> Result<bool, StateError> {
        self.is_vetoed_impl(account, trade_id, instrument, name)
            .await
    }
    async fn clear_veto(
        &self,
        account: Option<&str>,
        trade_id: &str,
        instrument: &str,
        name: &str,
    ) -> Result<bool, StateError> {
        self.clear_veto_impl(account, trade_id, instrument, name)
            .await
    }
    async fn block_prep(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        self.block_prep_impl(account, instrument, step, now, ttl_seconds)
            .await
    }
    async fn is_prep_blocked(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> Result<bool, StateError> {
        self.is_prep_blocked_impl(account, instrument, step).await
    }
    async fn clear_prep_block(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> Result<bool, StateError> {
        self.clear_prep_block_impl(account, instrument, step).await
    }
    async fn set_pause(
        &self,
        trade_id: &str,
        blackout_id: &str,
        reason: Option<&str>,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        self.set_pause_impl(trade_id, blackout_id, reason, now, ttl_seconds)
            .await
    }
    async fn list_pauses_for_trade(&self, trade_id: &str) -> Result<Vec<PauseEntry>, StateError> {
        self.list_pauses_for_trade_impl(trade_id).await
    }
    async fn clear_pause(&self, trade_id: &str, blackout_id: &str) -> Result<bool, StateError> {
        self.clear_pause_impl(trade_id, blackout_id).await
    }
    async fn set_news_window(
        &self,
        trade_id: &str,
        news_id: &str,
        reason: Option<&str>,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        self.set_news_window_impl(trade_id, news_id, reason, now, ttl_seconds)
            .await
    }
    async fn list_news_windows_for_trade(
        &self,
        trade_id: &str,
    ) -> Result<Vec<NewsEntry>, StateError> {
        self.list_news_windows_for_trade_impl(trade_id).await
    }
    async fn clear_news_window(&self, trade_id: &str, news_id: &str) -> Result<bool, StateError> {
        self.clear_news_window_impl(trade_id, news_id).await
    }
    async fn snapshot(&self) -> Result<Snapshot, StateError> {
        self.snapshot_impl().await
    }
    async fn record_entry_attempt(&self, attempt: EntryAttempt) -> Result<(), StateError> {
        self.record_entry_attempt_impl(attempt).await
    }
    async fn list_entry_attempts(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Vec<EntryAttempt>, StateError> {
        self.list_entry_attempts_impl(account, trade_id).await
    }
    async fn list_all_entry_attempts(&self) -> Result<Vec<EntryAttempt>, StateError> {
        self.list_all_entry_attempts_impl().await
    }
    async fn delete_entry_attempt(
        &self,
        account: Option<&str>,
        trade_id: &str,
        attempt_no: u32,
    ) -> Result<(), StateError> {
        self.delete_entry_attempt_impl(account, trade_id, attempt_no)
            .await
    }
    async fn set_entry_attempt_broker_trade_id(
        &self,
        account: Option<&str>,
        trade_id: &str,
        attempt_no: u32,
        broker_trade_id: &str,
    ) -> Result<(), StateError> {
        self.set_entry_attempt_broker_trade_id_impl(account, trade_id, attempt_no, broker_trade_id)
            .await
    }
    async fn is_retry_fire_seen(
        &self,
        account: Option<&str>,
        trade_id: &str,
        shell_time: DateTime<Utc>,
    ) -> Result<bool, StateError> {
        self.is_retry_fire_seen_impl(account, trade_id, shell_time)
            .await
    }
    async fn mark_retry_fire_seen(
        &self,
        account: Option<&str>,
        trade_id: &str,
        shell_time: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        self.mark_retry_fire_seen_impl(account, trade_id, shell_time, ttl_seconds)
            .await
    }
    async fn set_spread_blackout_window(
        &self,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        self.set_spread_blackout_window_impl(now, ttl_seconds).await
    }
    async fn get_spread_blackout_window(&self) -> Result<Option<SpreadBlackoutWindow>, StateError> {
        self.get_spread_blackout_window_impl().await
    }
    async fn set_blackout_windows(
        &self,
        instrument: &str,
        windows: &[NoEntryWindow],
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        self.set_blackout_windows_impl(instrument, windows, now, ttl_seconds)
            .await
    }
    async fn get_blackout_windows(
        &self,
        instrument: &str,
    ) -> Result<Vec<NoEntryWindow>, StateError> {
        self.get_blackout_windows_impl(instrument).await
    }
    async fn upsert_spread_blackout_record(
        &self,
        record: &SpreadBlackoutRecord,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        self.upsert_spread_blackout_record_impl(record, ttl_seconds)
            .await
    }
    async fn get_spread_blackout_record(
        &self,
        trade_id: &str,
    ) -> Result<Option<SpreadBlackoutRecord>, StateError> {
        self.get_spread_blackout_record_impl(trade_id).await
    }
    async fn list_all_spread_blackout_records(
        &self,
    ) -> Result<Vec<SpreadBlackoutRecord>, StateError> {
        self.list_all_spread_blackout_records_impl().await
    }
    async fn clear_spread_blackout_record(&self, trade_id: &str) -> Result<(), StateError> {
        self.clear_spread_blackout_record_impl(trade_id).await
    }
    async fn get_mw_state(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Option<MwState>, StateError> {
        self.get_mw_state_impl(account, trade_id).await
    }
    async fn upsert_mw_state(
        &self,
        account: Option<&str>,
        trade_id: &str,
        state: &MwState,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        self.upsert_mw_state_impl(account, trade_id, state, ttl_seconds)
            .await
    }
    async fn clear_mw_state(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        self.clear_mw_state_impl(account, trade_id).await
    }
    async fn put_trade_plan(
        &self,
        account: Option<&str>,
        plan: &TradePlan,
    ) -> Result<(), StateError> {
        self.put_trade_plan_impl(account, plan).await
    }
    async fn get_trade_plan(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Option<TradePlan>, StateError> {
        self.get_trade_plan_impl(account, trade_id).await
    }
    async fn list_all_trade_plans(&self) -> Result<Vec<StoredPlan>, StateError> {
        self.list_all_trade_plans_impl().await
    }
    async fn clear_trade_plan(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        self.clear_trade_plan_impl(account, trade_id).await
    }
    async fn get_plan_state(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Option<PlanState>, StateError> {
        self.get_plan_state_impl(account, trade_id).await
    }
    async fn put_plan_state(
        &self,
        account: Option<&str>,
        trade_id: &str,
        state: &PlanState,
    ) -> Result<(), StateError> {
        self.put_plan_state_impl(account, trade_id, state).await
    }
    async fn clear_plan_state(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        self.clear_plan_state_impl(account, trade_id).await
    }
    async fn record_control_event(
        &self,
        account: Option<&str>,
        trade_id: &str,
        event: &ControlEvent,
    ) -> Result<(), StateError> {
        self.record_control_event_impl(account, trade_id, event)
            .await
    }
    async fn list_control_events(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Vec<ControlEvent>, StateError> {
        self.list_control_events_impl(account, trade_id).await
    }
    async fn clear_control_events(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        self.clear_control_events_impl(account, trade_id).await
    }
    async fn archive_plan(
        &self,
        account: Option<&str>,
        plan: &TradePlan,
        final_state: &PlanState,
        archived_at: DateTime<Utc>,
    ) -> Result<(), StateError> {
        self.archive_plan_impl(account, plan, final_state, archived_at)
            .await
    }
    async fn list_all_archived_plans(&self) -> Result<Vec<ArchivedPlan>, StateError> {
        self.list_all_archived_plans_impl().await
    }
    async fn clear_archived_plan(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        self.clear_archived_plan_impl(account, trade_id).await
    }
}
