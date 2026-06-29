//! `trade-control-worker` — the native (VM + Postgres) runtime that replaces the
//! Cloudflare Worker. Phase 0 lands the Postgres-backed [`StateStore`]
//! (`PgStateStore`); later phases add the axum HTTP receiver and the tokio
//! scheduler. See `MIGRATION-VM-POSTGRES.md`.

mod pg;

pub use pg::PgStateStore;
