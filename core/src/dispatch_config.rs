//! [`DispatchConfig`] — the pre-resolved runtime configuration the entry
//! dispatch (`run_enter`) needs, decoupled from any backend.
//!
//! The Cloudflare worker's `run_enter` read four things straight off the
//! `worker::Env`: the worker-wide risk cap, the worker-wide max open
//! positions, the per-instrument pip size, and the per-account [`AccountCaps`].
//! That coupled the dispatch core to Cloudflare and blocked sharing it with the
//! native (VM + Postgres) runtime.
//!
//! This value object carries those four already-resolved at the **edge**, so
//! the dispatch itself is backend-agnostic:
//!
//! * the **wasm worker** builds it from `Env` secrets + the KV account index;
//! * the **native runtime** builds it from `Secrets` + the Postgres account
//!   index.
//!
//! Everything here is per-request: `pip_size` is resolved for *this intent's*
//! instrument and `caps` for *this intent's* account, both before the dispatch
//! is entered. The worker-wide scalars are constant per process but carried
//! here too so the dispatch never reaches back to a secret source.

use crate::account::AccountCaps;

/// Runtime configuration resolved for a single entry dispatch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DispatchConfig {
    /// Worker-wide max `risk_pct` per trade (the `MAX_RISK_PCT_PER_TRADE`
    /// secret / `Secrets::max_risk_pct`). The per-account [`caps`](Self::caps)
    /// can only narrow this, never relax it — see
    /// [`AccountCaps::resolve_max_risk_pct`].
    pub worker_max_risk_pct: f64,
    /// Worker-wide max simultaneous open positions (the `MAX_OPEN_POSITIONS`
    /// secret / `Secrets::max_open_positions`). Narrowed by
    /// [`AccountCaps::resolve_max_open_positions`].
    pub worker_max_open_positions: u32,
    /// Pip size resolved for this intent's instrument: the intent's baked
    /// `pip_size` is preferred at the call site; this is the fallback chain's
    /// result (per-instrument `PIP_SIZE_<INSTR>` override, then the forex
    /// default). Resolved at the edge so the dispatch needs no secret lookup.
    pub pip_size: f64,
    /// Tick size fallback for this intent's instrument, resolved at the edge
    /// (per-instrument override, else `None`). The baked `Intent::tick_size` is
    /// preferred at the call site; when both are absent the dispatch falls back
    /// to `pip_size`. `None` here means "no edge-resolved tick" — see the
    /// fallback chain in `dispatch::enter`.
    pub tick_size: Option<f64>,
    /// Per-account risk caps for this intent's account (default — all `None` —
    /// for an unnamed account or a missing record). Resolved against the
    /// worker-wide scalars above inside the dispatch.
    pub caps: AccountCaps,
}

impl DispatchConfig {
    /// The effective max `risk_pct` for this dispatch: the worker-wide cap
    /// narrowed by the account cap (never relaxed).
    pub fn effective_max_risk_pct(&self) -> f64 {
        self.caps.resolve_max_risk_pct(self.worker_max_risk_pct)
    }

    /// The effective max open positions for this dispatch: the worker-wide
    /// cap narrowed by the account cap (never relaxed).
    pub fn effective_max_open_positions(&self) -> u32 {
        self.caps
            .resolve_max_open_positions(self.worker_max_open_positions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> DispatchConfig {
        DispatchConfig {
            worker_max_risk_pct: 1.0,
            worker_max_open_positions: 3,
            pip_size: 0.0001,
            tick_size: None,
            caps: AccountCaps::default(),
        }
    }

    #[test]
    fn default_caps_pass_through_worker_wide() {
        let cfg = base();
        assert_eq!(cfg.effective_max_risk_pct(), 1.0);
        assert_eq!(cfg.effective_max_open_positions(), 3);
    }

    #[test]
    fn account_caps_narrow_but_never_relax() {
        let cfg = DispatchConfig {
            caps: AccountCaps {
                max_risk_pct: Some(0.5),
                max_open_positions: Some(1),
            },
            ..base()
        };
        assert_eq!(cfg.effective_max_risk_pct(), 0.5);
        assert_eq!(cfg.effective_max_open_positions(), 1);

        // A looser account cap is ignored — the worker-wide bound holds.
        let looser = DispatchConfig {
            caps: AccountCaps {
                max_risk_pct: Some(5.0),
                max_open_positions: Some(99),
            },
            ..base()
        };
        assert_eq!(looser.effective_max_risk_pct(), 1.0);
        assert_eq!(looser.effective_max_open_positions(), 3);
    }
}
