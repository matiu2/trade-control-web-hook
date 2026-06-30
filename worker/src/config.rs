//! Runtime configuration for the native worker.
//!
//! Two halves, by the user's Phase-1 decision (`MIGRATION-VM-POSTGRES.md`):
//!
//! * [`Config`] — non-secret settings from a **TOML file** (`config.toml`):
//!   the HTTP bind address, the Postgres URL, and the per-task scheduler
//!   intervals. Version-controllable; lives on the box.
//! * [`Secrets`] — sensitive values from **environment variables**: the HMAC
//!   signing key, the admin key, OANDA token + live flag, and the worker-wide
//!   risk caps. Never written to the TOML.
//!
//! Account *credentials* are not here — TradeNation logins come from the enc
//! account store (`~/.config/tradenation/accounts.enc`, resolved by name) and
//! account *metadata* from the Postgres `accounts` table
//! ([`crate::PgMetadataStore`]).

use std::time::Duration;

use serde::Deserialize;

/// Non-secret runtime configuration, deserialized from `config.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// HTTP receiver settings.
    #[serde(default)]
    pub http: HttpConfig,
    /// Database connection.
    pub database: DatabaseConfig,
    /// Per-task scheduler cadences.
    #[serde(default)]
    pub scheduler: SchedulerConfig,
}

/// HTTP receiver binding. The worker terminates **plain HTTP on loopback**; a
/// reverse proxy (nginx/caddy) in front handles TLS (user decision #1).
#[derive(Debug, Clone, Deserialize)]
pub struct HttpConfig {
    /// Address to bind, e.g. `127.0.0.1`. Loopback by default so the worker is
    /// only reachable through the proxy.
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    /// Port to bind.
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            port: default_port(),
        }
    }
}

fn default_bind_addr() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8787
}

/// Postgres connection. The URL itself is non-secret operationally (it points
/// at a loopback/VPC DB), so it lives in the TOML; if a deployment needs the
/// password kept out of the file, set `database.url` to an env-var reference
/// resolved by the operator's process manager.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// `postgresql://user:pass@host:port/db`.
    pub url: String,
    /// Max pool connections. Defaults to 10 (matches [`crate::PgStateStore`]).
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_max_connections() -> u32 {
    10
}

/// Per-task scheduler cadences (seconds). Each upkeep task runs on its own
/// tokio `interval` at its natural cadence (user decision #2) rather than one
/// fixed `*/N` list. Defaults mirror the Cloudflare cron behaviour: the
/// frequent upkeep ran every 15 min; the daily jobs self-gated on the hour.
#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    /// Engine tick — evaluate every registered plan against fresh candles and
    /// dispatch fired intents. The CF worker ran this on the 15-min cron; the
    /// migration plan bumps it faster once proven (Stage F). Default 60s.
    #[serde(default = "default_engine_secs")]
    pub engine_secs: u64,
    /// Session refresh + order sweep + spread-recovery watch + breakeven watch.
    /// The "every 15-min tick" bundle. Default 900s.
    #[serde(default = "default_upkeep_secs")]
    pub upkeep_secs: u64,
    /// Daily jobs (NY-close spread-blackout apply + market-hours blackout
    /// refresh). The task wakes this often and self-gates on the hour/minute,
    /// same as the CF worker. Default 900s.
    #[serde(default = "default_daily_tick_secs")]
    pub daily_tick_secs: u64,
    /// Expired-row GC ([`PgStateStore::gc_expired`](crate::PgStateStore::gc_expired))
    /// — `DELETE FROM … WHERE expires_at < now()` across every TTL table, the
    /// native stand-in for KV's automatic TTL eviction. Reads already filter
    /// `expires_at > now()`, so this is pure housekeeping, not correctness.
    /// Native-only. Default 3600s (hourly).
    #[serde(default = "default_expiry_sweep_secs")]
    pub expiry_sweep_secs: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            engine_secs: default_engine_secs(),
            upkeep_secs: default_upkeep_secs(),
            daily_tick_secs: default_daily_tick_secs(),
            expiry_sweep_secs: default_expiry_sweep_secs(),
        }
    }
}

fn default_engine_secs() -> u64 {
    60
}
fn default_upkeep_secs() -> u64 {
    900
}
fn default_daily_tick_secs() -> u64 {
    900
}
fn default_expiry_sweep_secs() -> u64 {
    3600
}

impl SchedulerConfig {
    /// The engine-tick interval as a [`Duration`].
    pub fn engine_interval(&self) -> Duration {
        Duration::from_secs(self.engine_secs)
    }
    /// The frequent-upkeep interval as a [`Duration`].
    pub fn upkeep_interval(&self) -> Duration {
        Duration::from_secs(self.upkeep_secs)
    }
    /// The daily-tick interval as a [`Duration`].
    pub fn daily_tick_interval(&self) -> Duration {
        Duration::from_secs(self.daily_tick_secs)
    }
    /// The expiry-sweep interval as a [`Duration`].
    pub fn expiry_sweep_interval(&self) -> Duration {
        Duration::from_secs(self.expiry_sweep_secs)
    }
}

/// Failure modes for loading [`Config`].
#[derive(Debug)]
pub enum ConfigError {
    /// The TOML file couldn't be read.
    Read {
        path: String,
        source: std::io::Error,
    },
    /// The TOML failed to parse / didn't match the schema.
    Parse {
        path: String,
        source: toml::de::Error,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "reading config {path}: {source}"),
            Self::Parse { path, source } => write!(f, "parsing config {path}: {source}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Load and parse a `config.toml` from `path`.
    pub fn load(path: &str) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_string(),
            source,
        })?;
        Self::from_toml(&text, path)
    }

    /// Parse config from a TOML string (testable without touching the FS).
    pub fn from_toml(text: &str, path: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_uses_defaults() {
        // Only the required `database.url` supplied; everything else defaults.
        let toml = r#"
            [database]
            url = "postgresql://localhost/trade_control"
        "#;
        let cfg = Config::from_toml(toml, "test").unwrap();
        assert_eq!(cfg.database.url, "postgresql://localhost/trade_control");
        assert_eq!(cfg.database.max_connections, 10);
        assert_eq!(cfg.http.bind_addr, "127.0.0.1");
        assert_eq!(cfg.http.port, 8787);
        assert_eq!(cfg.scheduler.engine_secs, 60);
        assert_eq!(cfg.scheduler.upkeep_secs, 900);
        assert_eq!(cfg.scheduler.expiry_sweep_secs, 3600);
    }

    #[test]
    fn full_config_overrides_every_field() {
        let toml = r#"
            [http]
            bind_addr = "0.0.0.0"
            port = 9000

            [database]
            url = "postgresql://db/x"
            max_connections = 25

            [scheduler]
            engine_secs = 5
            upkeep_secs = 300
            daily_tick_secs = 600
            expiry_sweep_secs = 1800
        "#;
        let cfg = Config::from_toml(toml, "test").unwrap();
        assert_eq!(cfg.http.bind_addr, "0.0.0.0");
        assert_eq!(cfg.http.port, 9000);
        assert_eq!(cfg.database.max_connections, 25);
        assert_eq!(cfg.scheduler.engine_interval(), Duration::from_secs(5));
        assert_eq!(cfg.scheduler.upkeep_interval(), Duration::from_secs(300));
        assert_eq!(
            cfg.scheduler.daily_tick_interval(),
            Duration::from_secs(600)
        );
        assert_eq!(
            cfg.scheduler.expiry_sweep_interval(),
            Duration::from_secs(1800)
        );
    }

    #[test]
    fn missing_database_url_is_an_error() {
        // `database` is required (no default) — a config without it must fail
        // loudly rather than silently point at nothing.
        let toml = r#"
            [http]
            port = 8000
        "#;
        let err = Config::from_toml(toml, "test").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn malformed_toml_is_a_parse_error() {
        let err = Config::from_toml("this is not = = toml", "test").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }
}
