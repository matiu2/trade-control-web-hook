//! `trade-control-worker` — the native (VM + Postgres) runtime that replaces the
//! Cloudflare Worker. Phase 0 lands the Postgres-backed [`StateStore`]
//! (`PgStateStore`); Phase 1 adds the account metadata store, runtime config,
//! the axum HTTP receiver, and the tokio scheduler. See `MIGRATION-VM-POSTGRES.md`.

mod broker_factory;
mod config;
mod dispatch_config_native;
mod engine_health;
mod entry_notifications;
pub mod entry_reconciliation;
pub mod http;
mod manual_close_audit;
mod native_cron;
mod operation_lock;
mod operator_alert;
mod paper_entries;
mod pg;
mod pg_accounts;
mod plan_admin;
mod position_links;
mod position_tracking;
pub mod recording_pg;
mod scheduler;
mod secrets;
mod trade_notifications;

pub use broker_factory::{
    BrokerError, acquire_ctrader, acquire_ibkr, acquire_mt5, acquire_oanda, acquire_tn,
    validate_ctrader_config,
};
pub use config::{Config, ConfigError, DatabaseConfig, HttpConfig, SchedulerConfig};
pub use dispatch_config_native::build_dispatch_config_native;
pub use native_cron::NativeCronEnv;
pub use pg::PgStateStore;
pub use pg_accounts::PgMetadataStore;
pub use scheduler::run_scheduler;
pub use secrets::{
    DEFAULT_MAX_OPEN_POSITIONS, DEFAULT_MAX_RISK_PCT, DEFAULT_PIP_SIZE, Secrets, SecretsError,
};

pub mod account_capabilities;
pub mod attempt_recording;
