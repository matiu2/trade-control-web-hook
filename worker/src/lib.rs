//! `trade-control-worker` — the native (VM + Postgres) runtime that replaces the
//! Cloudflare Worker. Phase 0 lands the Postgres-backed [`StateStore`]
//! (`PgStateStore`); Phase 1 adds the account metadata store, the axum HTTP
//! receiver, and the tokio scheduler. See `MIGRATION-VM-POSTGRES.md`.

mod pg;
mod pg_accounts;

pub use pg::PgStateStore;
pub use pg_accounts::PgMetadataStore;
