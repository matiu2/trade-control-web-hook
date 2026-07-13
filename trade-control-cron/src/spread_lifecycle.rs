//! The live cron's driver for the SHARED resting-order lifecycle
//! ([`trade_control_core::pending_lifecycle::pending_order_lifecycle`]) — the
//! same function the offline replay calls, so cancel/restore of resting entry
//! orders (System 3) runs identically live and in replay (PR 2 cutover).
//!
//! This module owns exactly the live↔shared glue:
//!
//! - [`CronEnterConfigProvider`] — the live [`EnterConfigProvider`], forwarding
//!   to [`CronEnv::dispatch_config`] so `run_enter` stays backend-free.
//! - [`run_spread_lifecycle_for_account`] — acquires the account's
//!   [`BrokerHandle`], **matches it ONCE** to a single `impl Broker` (the shared
//!   fn is generic over `B: Broker`, which the enum can't satisfy), builds the
//!   live [`SignedBodySource`] (VerifiedSource) from the signing key, and calls
//!   `pending_order_lifecycle` for that account.
//!
//! ## What this REPLACES
//!
//! The old hand-rolled per-account cancel loop (`blackout_cancel::cancel_account`
//! with its `BrokerHandle` match helpers) and the old resting-order re-drive
//! inside `blackout_watch::watch_one` (via `crate::restore_cancelled_orders`) are
//! both gone; the shared fn is the single source of the cancel/restore decision.
//!
//! ## The ON-trigger delta (the point of PR 2)
//!
//! The old cancel sampled a LIVE QUOTE and cancelled only when
//! `spread_pips > elevated_threshold`. The shared `cancel_pass` uses the pure
//! baked-clock `is_spread_hour` predicate (the ON side of the ON/OFF asymmetry) —
//! deterministic, identical to replay, no quote round-trip on the ON side.
//!
//! ## Clear ownership (Option A)
//!
//! Called with [`ClearPolicy::LeaveForCaller`]: the shared fn restores System 3
//! but does NOT delete the record. The live watcher ([`crate::watch_recovery`])
//! restores System 2 (widened open-position stops) alongside and issues the
//! single record `clear` itself, preserving the coexistence contract ("restore
//! both, clear once"). Replay is the sole owner and passes `ClearRecord`.

use chrono::{DateTime, Utc};
use trade_control_core::dispatch_config::DispatchConfig;
use trade_control_core::incoming::Verified;
use trade_control_core::pending_lifecycle::{
    ClearPolicy, EnterConfigProvider, LifecycleReport, SignedBodySource, pending_order_lifecycle,
};

use crate::broker_handle::BrokerHandle;
use crate::seam::CronEnv;

/// The live [`EnterConfigProvider`]: resolve the per-enter [`DispatchConfig`] at
/// this edge via [`CronEnv::dispatch_config`] (risk caps, pip/tick fallback,
/// per-account caps), so a restored enter sizes identically to a first-run one
/// and `run_enter` stays backend-free.
struct CronEnterConfigProvider<'c, C: CronEnv> {
    cron: &'c C,
}

impl<C: CronEnv> EnterConfigProvider for CronEnterConfigProvider<'_, C> {
    async fn dispatch_config(&self, verified: &Verified) -> DispatchConfig {
        self.cron.dispatch_config(verified).await
    }
}

/// Run the shared resting-order lifecycle for ONE account: cancel resting orders
/// that entered a spread hour (baked clock) and restore records whose trough has
/// lifted, leaving the record for the caller to clear (Option A).
///
/// Returns the [`LifecycleReport`] for the caller to log / act on. `None` broker
/// acquisition or a missing signing key skip the account (logged) — the shared fn
/// needs both. The single `BrokerHandle` → `impl Broker` match lives here.
pub async fn run_spread_lifecycle_for_account<S, C>(
    store: &S,
    cron: &C,
    account: Option<&str>,
    now: DateTime<Utc>,
    clear: ClearPolicy,
) -> LifecycleReport
where
    S: trade_control_core::state::StateStore,
    C: CronEnv,
{
    let scope = account.unwrap_or("<global>");
    // The signing key is needed to re-verify a stored body on both the cancel
    // (RAIL 2/3) and restore (RAIL 7) sides; without it we can't trust a body, so
    // skip the whole account rather than cancel orders we can't book for restore.
    let Some(key) = cron.signing_key() else {
        tracing::error!("spread-lifecycle[{scope}]: no signing key; skipping account");
        return LifecycleReport::default();
    };
    let Some(broker) = cron.acquire_broker(account).await else {
        tracing::error!("spread-lifecycle[{scope}]: broker acquisition failed; skipping account");
        return LifecycleReport::default();
    };
    let src = SignedBodySource { key: &key };
    let cfg_provider = CronEnterConfigProvider { cron };

    // Match the enum to ONE concrete `impl Broker` (the shared fn is generic over
    // `B: Broker`; an enum isn't `impl Broker`). The two arms are identical apart
    // from the broker type.
    match &broker {
        BrokerHandle::Oanda(b) => {
            pending_order_lifecycle(b, store, &cfg_provider, &src, account, now, clear).await
        }
        BrokerHandle::TradeNation(b) => {
            pending_order_lifecycle(b, store, &cfg_provider, &src, account, now, clear).await
        }
    }
}
