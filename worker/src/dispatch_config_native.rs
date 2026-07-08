//! Native edge resolver for [`DispatchConfig`].
//!
//! The wasm worker built its [`DispatchConfig`] off the Cloudflare `Env`
//! (`build_dispatch_config` in `src/lib.rs`): the worker-wide risk caps from
//! secrets, the per-instrument pip-size fallback from a `PIP_SIZE_<INSTR>`
//! secret, and the per-account [`AccountCaps`] from the KV account index. This
//! is the native parallel: the same four values resolved from [`Secrets`] +
//! the Postgres account record's caps.
//!
//! `pip_size` here is only the *fallback* (the per-instrument override →
//! [`DEFAULT_PIP_SIZE`]); `run_enter` still prefers the intent's baked
//! `pip_size` over it, exactly as in the wasm worker.

use trade_control_core::account::AccountCaps;
use trade_control_core::dispatch_config::DispatchConfig;

use crate::{DEFAULT_PIP_SIZE, Secrets};

/// Resolve the [`DispatchConfig`] for an intent at the native edge.
///
/// `instrument` selects the pip-size fallback; `caps` are the per-account caps
/// from the resolved account metadata (default — all `None` — for an account
/// with no narrowing caps). The intent's baked `pip_size` still wins inside
/// `run_enter`; this only supplies the fallback.
pub fn build_dispatch_config_native(
    secrets: &Secrets,
    instrument: &str,
    caps: AccountCaps,
) -> DispatchConfig {
    DispatchConfig {
        worker_max_risk_pct: secrets.max_risk_pct,
        worker_max_open_positions: secrets.max_open_positions as u32,
        pip_size: secrets
            .pip_size_override(instrument)
            .unwrap_or(DEFAULT_PIP_SIZE),
        // No edge-side tick override (no `TICK_SIZE_<INSTR>` secret): the baked
        // `Intent::tick_size` from tv-arm is the authority, falling back to
        // `pip_size` inside `run_enter` when absent. `None` = no edge tick.
        tick_size: None,
        caps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets() -> Secrets {
        Secrets {
            signing_key: "sk".into(),
            admin_key: "ak".into(),
            max_risk_pct: 0.75,
            max_open_positions: 4.0,
            oanda_api_key: None,
            oanda_live: false,
        }
    }

    #[test]
    fn carries_caps_and_scalars() {
        let cfg = build_dispatch_config_native(&secrets(), "EUR_USD", AccountCaps::default());
        assert_eq!(cfg.worker_max_risk_pct, 0.75);
        assert_eq!(cfg.worker_max_open_positions, 4);
        // No `PIP_SIZE_EUR_USD` env override in the test process → forex default.
        assert_eq!(cfg.pip_size, DEFAULT_PIP_SIZE);
        assert_eq!(cfg.caps, AccountCaps::default());
    }
}
