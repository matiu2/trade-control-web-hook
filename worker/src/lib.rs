//! `trade-control-worker` — the native (VM + Postgres) runtime that replaces the
//! Cloudflare Worker. Phase 0 lands the Postgres-backed [`StateStore`]
//! (`PgStateStore`); Phase 1 adds the account metadata store, runtime config,
//! the axum HTTP receiver, and the tokio scheduler. See `MIGRATION-VM-POSTGRES.md`.

mod config;
mod pg;
mod pg_accounts;
mod secrets;

pub use config::{Config, ConfigError, DatabaseConfig, HttpConfig, SchedulerConfig};
pub use pg::PgStateStore;
pub use pg_accounts::PgMetadataStore;
pub use secrets::{
    DEFAULT_MAX_OPEN_POSITIONS, DEFAULT_MAX_RISK_PCT, DEFAULT_PIP_SIZE, Secrets, SecretsError,
};
