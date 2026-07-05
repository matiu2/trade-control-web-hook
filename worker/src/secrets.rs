//! Sensitive runtime values, loaded from **environment variables** (user
//! Phase-1 decision #4/#5). These are the native equivalents of the Cloudflare
//! Worker secrets enumerated in the dispatch-surface map; they never go in the
//! TOML [`crate::config::Config`].
//!
//! | env var | required | default | purpose |
//! |---|---|---|---|
//! | `SIGNING_KEY` | yes | — | HMAC key verifying signed intent bodies; also the `X-Diag-Key`. |
//! | `ADMIN_KEY` | yes | — | auth for the `/admin/*` write routes (`X-Admin-Key`). |
//! | `MAX_RISK_PCT_PER_TRADE` | no | `1.0` | worker-wide risk cap %. |
//! | `MAX_OPEN_POSITIONS` | no | `3` | worker-wide max open positions. |
//! | `OANDA_API_KEY` | no* | — | OANDA bearer token (shared across sub-accounts). |
//! | `OANDA_LIVE` | no | `false` | global live/practice flag; `"true"` → live. |
//!
//! `*` OANDA_API_KEY is only required if any OANDA account is configured; a
//! TradeNation-only deployment can omit it. Per-instrument `PIP_SIZE_<INSTR>`
//! overrides are read lazily on demand (there's an open set of them), not
//! eagerly into this struct — see [`Secrets::pip_size_override`].
//!
//! Per-account credentials (`TN_ACCOUNT_*` / `OANDA_ACCOUNT_*` in the CF worker)
//! are NOT here: TradeNation logins resolve from the enc account store, OANDA
//! sub-account ids from the Postgres account index.

/// Default worker-wide risk cap when `MAX_RISK_PCT_PER_TRADE` is unset. Matches
/// the legacy worker (`src/lib.rs`, `secret_or_default(…, 1.0)`).
pub const DEFAULT_MAX_RISK_PCT: f64 = 1.0;
/// Default worker-wide max open positions when `MAX_OPEN_POSITIONS` is unset.
pub const DEFAULT_MAX_OPEN_POSITIONS: f64 = 3.0;
/// Default pip size when no `PIP_SIZE_<INSTR>` override and the intent bakes
/// none. Matches the legacy worker's `DEFAULT_PIP_SIZE`.
pub const DEFAULT_PIP_SIZE: f64 = 0.0001;

/// Env-var name for the per-instrument pip-size override prefix.
const PIP_SIZE_PREFIX: &str = "PIP_SIZE_";

/// The sensitive runtime values, resolved from the process environment at
/// startup.
#[derive(Debug, Clone)]
pub struct Secrets {
    /// HMAC key for verifying signed intent bodies (and the diag-route key).
    pub signing_key: String,
    /// Auth key for the `/admin/*` write routes.
    pub admin_key: String,
    /// Worker-wide max risk % per trade.
    pub max_risk_pct: f64,
    /// Worker-wide max simultaneous open positions.
    pub max_open_positions: f64,
    /// OANDA bearer token, if any OANDA account is configured.
    pub oanda_api_key: Option<String>,
    /// Global OANDA live/practice flag (named accounts override via `kind`).
    pub oanda_live: bool,
}

/// Failure modes for loading [`Secrets`].
#[derive(Debug)]
pub enum SecretsError {
    /// A required env var is absent or empty.
    Missing(&'static str),
    /// A numeric env var didn't parse.
    Parse { var: &'static str, value: String },
}

impl std::fmt::Display for SecretsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(var) => write!(f, "missing required env var {var}"),
            Self::Parse { var, value } => {
                write!(f, "env var {var} is not a valid number: {value:?}")
            }
        }
    }
}

impl std::error::Error for SecretsError {}

impl Secrets {
    /// Load secrets from the process environment. Required vars missing → error;
    /// optional numeric vars fall back to their documented defaults; a numeric
    /// var that is *present but unparseable* is a hard error (a typo in a risk
    /// cap must not silently fall back to the looser default).
    pub fn from_env() -> Result<Self, SecretsError> {
        let signing_key = required("SIGNING_KEY")?;
        let admin_key = required("ADMIN_KEY")?;
        let max_risk_pct = parse_or_default("MAX_RISK_PCT_PER_TRADE", DEFAULT_MAX_RISK_PCT)?;
        let max_open_positions =
            parse_or_default("MAX_OPEN_POSITIONS", DEFAULT_MAX_OPEN_POSITIONS)?;
        let oanda_api_key = optional("OANDA_API_KEY");
        let oanda_live = optional("OANDA_LIVE")
            .map(|s| s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        Ok(Self {
            signing_key,
            admin_key,
            max_risk_pct,
            max_open_positions,
            oanda_api_key,
            oanda_live,
        })
    }

    /// Per-instrument pip-size override from `PIP_SIZE_<INSTRUMENT>` (e.g.
    /// `PIP_SIZE_USD_JPY`). Read lazily because the set of instruments is open.
    /// `None` when unset (caller falls back to the intent's baked pip size, then
    /// [`DEFAULT_PIP_SIZE`]).
    pub fn pip_size_override(&self, instrument: &str) -> Option<f64> {
        let var = format!("{PIP_SIZE_PREFIX}{instrument}");
        std::env::var(&var).ok().and_then(|v| v.parse::<f64>().ok())
    }
}

/// Read a required env var; empty or absent is [`SecretsError::Missing`].
fn required(var: &'static str) -> Result<String, SecretsError> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(SecretsError::Missing(var)),
    }
}

/// Read an optional env var; absent or empty → `None`.
fn optional(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.is_empty())
}

/// Parse a numeric env var, falling back to `default` when absent/empty but
/// erroring when present-and-unparseable.
fn parse_or_default(var: &'static str, default: f64) -> Result<f64, SecretsError> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => v
            .parse::<f64>()
            .map_err(|_| SecretsError::Parse { var, value: v }),
        _ => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate process env, so they must not run concurrently with
    // each other. A module-level mutex serialises them. (Other test binaries
    // don't touch these vars.)
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_all() {
        for v in [
            "SIGNING_KEY",
            "ADMIN_KEY",
            "MAX_RISK_PCT_PER_TRADE",
            "MAX_OPEN_POSITIONS",
            "OANDA_API_KEY",
            "OANDA_LIVE",
        ] {
            unsafe { std::env::remove_var(v) };
        }
    }

    #[test]
    fn loads_required_and_defaults_optional() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        unsafe {
            std::env::set_var("SIGNING_KEY", "sk");
            std::env::set_var("ADMIN_KEY", "ak");
        }
        let s = Secrets::from_env().unwrap();
        assert_eq!(s.signing_key, "sk");
        assert_eq!(s.admin_key, "ak");
        assert_eq!(s.max_risk_pct, DEFAULT_MAX_RISK_PCT);
        assert_eq!(s.max_open_positions, DEFAULT_MAX_OPEN_POSITIONS);
        assert!(s.oanda_api_key.is_none());
        assert!(!s.oanda_live);
        clear_all();
    }

    #[test]
    fn missing_signing_key_errors() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        unsafe { std::env::set_var("ADMIN_KEY", "ak") };
        let err = Secrets::from_env().unwrap_err();
        assert!(matches!(err, SecretsError::Missing("SIGNING_KEY")));
        clear_all();
    }

    #[test]
    fn unparseable_risk_cap_is_a_hard_error() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        unsafe {
            std::env::set_var("SIGNING_KEY", "sk");
            std::env::set_var("ADMIN_KEY", "ak");
            std::env::set_var("MAX_RISK_PCT_PER_TRADE", "loose");
        }
        let err = Secrets::from_env().unwrap_err();
        assert!(matches!(
            err,
            SecretsError::Parse {
                var: "MAX_RISK_PCT_PER_TRADE",
                ..
            }
        ));
        clear_all();
    }

    #[test]
    fn oanda_live_true_is_case_insensitive() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        unsafe {
            std::env::set_var("SIGNING_KEY", "sk");
            std::env::set_var("ADMIN_KEY", "ak");
            std::env::set_var("OANDA_LIVE", "TRUE");
            std::env::set_var("OANDA_API_KEY", "tok");
        }
        let s = Secrets::from_env().unwrap();
        assert!(s.oanda_live);
        assert_eq!(s.oanda_api_key.as_deref(), Some("tok"));
        clear_all();
    }
}
