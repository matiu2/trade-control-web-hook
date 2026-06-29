//! `trade-control-worker` — the native (VM + Postgres) runtime that replaces the
//! Cloudflare Worker. Phase 0 lands the Postgres-backed [`StateStore`]
//! (`PgStateStore`); Phase 1 adds the account metadata store, runtime config,
//! the axum HTTP receiver, and the tokio scheduler. See `MIGRATION-VM-POSTGRES.md`.

mod broker_factory;
mod config;
mod dispatch_config_native;
pub mod http;
mod native_cron;
mod pg;
mod pg_accounts;
mod scheduler;
mod secrets;

pub use broker_factory::{BrokerError, acquire_oanda, acquire_tn};
pub use config::{Config, ConfigError, DatabaseConfig, HttpConfig, SchedulerConfig};
pub use dispatch_config_native::build_dispatch_config_native;
pub use native_cron::NativeCronEnv;
pub use pg::PgStateStore;
pub use pg_accounts::PgMetadataStore;
pub use scheduler::run_scheduler;
pub use secrets::{
    DEFAULT_MAX_OPEN_POSITIONS, DEFAULT_MAX_RISK_PCT, DEFAULT_PIP_SIZE, Secrets, SecretsError,
};
