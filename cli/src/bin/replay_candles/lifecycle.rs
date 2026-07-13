//! The offline seams the shared `core::pending_order_lifecycle` needs when it's
//! driven from the replay loop (PR 4b-3).
//!
//! The lifecycle is generic over a [`Broker`], a [`StateStore`], an
//! [`EnterConfigProvider`], and a [`VerifiedSource`]. The replay already supplies
//! the first two (`ReplayBroker` + `MemStateStore`); this module fills the last
//! two so a spread-hour resting order is cancelled — and later re-driven —
//! through the SAME engine→broker path the live cron uses, not a replay-only
//! branch. Keeping the decision in `core` is what makes replay reproduce the live
//! cancel (`[[strategy_changes_in_both_replayer_and_worker]]`).

use chrono::{DateTime, Utc};
use trade_control_core::dispatch_config::DispatchConfig;
use trade_control_core::incoming::Verified;
use trade_control_core::pending_lifecycle::{EnterConfigProvider, Recovered, VerifiedSource};

use super::replay_broker::ReplayBroker;

/// The replay [`EnterConfigProvider`]: a fixed offline [`DispatchConfig`] with the
/// worker defaults (`worker_max_risk_pct` 1.0, `worker_max_open_positions` 3), the
/// plan's baked `pip_size`, no edge-resolved tick, and default (no-narrowing)
/// account caps — exactly what `dispatch_enter` builds for the live-style enter
/// dispatch in the replay. So a re-driven enter sizes identically to a first-run
/// one.
pub struct ReplayConfigProvider {
    pip_size: f64,
}

impl ReplayConfigProvider {
    pub fn new(pip_size: f64) -> Self {
        Self { pip_size }
    }
}

impl EnterConfigProvider for ReplayConfigProvider {
    async fn dispatch_config(&self, _verified: &Verified) -> DispatchConfig {
        DispatchConfig {
            worker_max_risk_pct: 1.0,
            worker_max_open_positions: 3,
            pip_size: self.pip_size,
            // The baked `Intent::tick_size` takes precedence inside `run_enter`,
            // falling back to `pip_size` — the same chain the worker uses.
            tick_size: None,
            caps: Default::default(),
        }
    }
}

/// The replay [`VerifiedSource`]: hand back the [`Verified`] the fake broker was
/// *armed* with when it "placed" the order, keyed by `order_id`, ignoring the
/// (absent) signed body. This is the offline seam — the replay has the intent+shell
/// in hand already, so the lifecycle re-drives with NO HMAC round-trip and no
/// stored body. Mirrors `core`'s test-only `ArmedSource`, reading the live
/// `ReplayBroker` ledger instead of a hand-built map.
pub struct ReplayVerifiedSource<'b> {
    broker: &'b ReplayBroker,
}

impl<'b> ReplayVerifiedSource<'b> {
    pub fn new(broker: &'b ReplayBroker) -> Self {
        Self { broker }
    }
}

impl VerifiedSource for ReplayVerifiedSource<'_> {
    async fn recover(
        &self,
        order_id: &str,
        _signed_body: Option<&str>,
        _now: DateTime<Utc>,
    ) -> Recovered {
        match self.broker.armed_verified(order_id) {
            Some(v) => Recovered::Ok(Box::new(v)),
            // RAIL 2: no armed payload ⇒ never cancel what we can't restore.
            None => Recovered::Unrecoverable,
        }
    }
}
