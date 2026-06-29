//! wasm-worker broker-acquisition plumbing for the cron jobs.
//!
//! The **sweep job itself** (`sweep_pending_orders` + its per-attempt
//! cancel/close logic) moved to the shared `trade-control-cron` crate so the
//! wasm Cloudflare worker and the native VM scheduler run the *same* sweep code
//! (the `[[strategy_changes_in_both_replayer_and_worker]]` discipline). What
//! stays here is the wasm-specific glue the [`EnvCronEnv`](super::seam) impl
//! needs:
//!
//! * [`open_store`] — open the KV-backed [`KvStateStore`]; `scheduled()` calls
//!   it once and reuses the store for every shared cron job.
//! * [`acquire_broker_for_account`] — pick a broker for an account from the KV
//!   account index + `Env` secrets. `EnvCronEnv::acquire_broker` delegates here.
//! * [`resolve_broker_kind`] — the KV/metadata lookup behind it.
//!
//! These reference Cloudflare's `worker::Env`, so they can't live in the
//! worker-free shared crate; the native runtime's `NativeCronEnv` fills the
//! same seam from Postgres + `Secrets` instead.

use trade_control_core::intent::BrokerKind;

use crate::state::KvStateStore;

/// Open the KV-backed state store. Shared with the spread-blackout
/// cron steps (`blackout_apply`, `blackout_watch`) and the order sweep.
pub(crate) fn open_store(env: &worker::Env) -> Option<KvStateStore> {
    match env.kv(crate::KV_NAMESPACE) {
        Ok(kv) => Some(KvStateStore::new(kv)),
        Err(err) => {
            rlog_err!("cron sweep: KV binding missing: {err:?}");
            None
        }
    }
}

// `BrokerHandle` lives in the shared `trade-control-cron` crate (so the engine
// tick is worker-free). Re-exported here under its old path because the
// `EnvCronEnv` seam impl matches on it.
pub(crate) use trade_control_cron::BrokerHandle;

/// Pick a broker for `account`. `None` → worker-global OANDA (matches
/// the existing fetch-path default). `Some(name)` → the account's
/// broker kind, looked up from metadata. The [`EnvCronEnv`](super::seam)
/// `acquire_broker` seam method delegates to this.
pub(crate) async fn acquire_broker_for_account(
    env: &worker::Env,
    account: Option<&str>,
) -> Option<BrokerHandle> {
    let broker_kind = resolve_broker_kind(env, account).await?;
    match broker_kind {
        BrokerKind::Oanda => crate::acquire_oanda_broker(env, account)
            .await
            .map(BrokerHandle::Oanda),
        BrokerKind::TradeNation => crate::acquire_tn_broker(env, account)
            .await
            .map(|b| BrokerHandle::TradeNation(crate::tradenation_adapter::TradeNationAdapter(b))),
    }
}

/// Resolve broker kind from the account metadata. Returns:
/// * `Some(Oanda)` when the attempt is unnamed (worker-global) — the
///   fetch-path treats `account: None` as the global OANDA account.
/// * `Some(kind)` on a successful metadata lookup.
/// * `None` on the native test target, or when KV / metadata lookup
///   fails — `None` is logged by the caller and the row is skipped
///   rather than silently misrouted to the wrong broker. PR A had a
///   non-wasm `BrokerKind::Oanda` fallback here that would have routed
///   TN accounts to OANDA in tests.
async fn resolve_broker_kind(env: &worker::Env, account: Option<&str>) -> Option<BrokerKind> {
    let Some(name) = account else {
        return Some(BrokerKind::Oanda);
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (env, name);
        None
    }
    #[cfg(target_arch = "wasm32")]
    {
        use trade_control_core::account::MetadataStore;
        let kv = match env.kv(crate::KV_NAMESPACE) {
            Ok(kv) => kv,
            Err(err) => {
                rlog_err!("cron sweep[{name}]: KV binding missing: {err:?}");
                return None;
            }
        };
        let metadata = crate::accounts::KvMetadataStore::new(kv);
        match metadata.get(name).await {
            Ok(m) => Some(m.broker),
            Err(err) => {
                rlog_err!("cron sweep[{name}]: metadata lookup failed: {err}");
                None
            }
        }
    }
}
