//! The native [`CronEnv`] — the VM runtime's side of the shared cron seam.
//!
//! The shared engine tick ([`trade_control_cron::run_engine_tick`]) is generic
//! over the [`CronEnv`](trade_control_cron::CronEnv) trait: three
//! backend-specific operations (broker acquisition, dispatch-config resolution,
//! tick recording) the engine can't do itself. The wasm worker fills them from
//! Cloudflare `&Env`; [`NativeCronEnv`] fills them from the Postgres account
//! index ([`PgMetadataStore`]) + process [`Secrets`], reusing the same
//! `acquire_oanda` / `acquire_tn` / `build_dispatch_config_native` helpers the
//! HTTP receiver uses.
//!
//! It holds an `Arc<AppState>` so it shares the receiver's pool/secrets exactly
//! (one connection pool, one secrets snapshot). The cron runs on the same
//! local-thread runtime the HTTP dispatcher uses (see [`crate::scheduler`]), so
//! the `?Send` broker futures are legal here.

use std::sync::Arc;

use trade_control_core::account::{AccountMetadata, MetadataStore};
use trade_control_core::dispatch_config::DispatchConfig;
use trade_control_core::incoming::Verified;
use trade_control_core::intent::BrokerKind;
use trade_control_core::tick_bundle::TickBundle;
use trade_control_cron::{BrokerHandle, CronEnv};

use crate::http::AppState;
use crate::{acquire_oanda, acquire_tn, build_dispatch_config_native};

/// Native [`CronEnv`]: resolves the engine's three backend ops from Postgres +
/// `Secrets`. Cheap to clone (an `Arc` bump); the scheduler holds one and drives
/// every engine tick through it.
#[derive(Clone)]
pub struct NativeCronEnv {
    state: Arc<AppState>,
}

impl NativeCronEnv {
    /// Build from the shared [`AppState`] (the same one the HTTP receiver owns),
    /// so the cron and the receiver share one Postgres pool and one secrets
    /// snapshot.
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Resolve the account metadata for a plan's account, logging + returning
    /// `None` on any miss (so the engine skips this plan for the tick rather than
    /// misrouting it). A `None` account name has no credentials to route to yet
    /// — the native runtime requires a named account for broker actions, exactly
    /// as the HTTP receiver does (`AccountResolveError::Required`).
    async fn resolve_meta(&self, account: Option<&str>) -> Option<AccountMetadata> {
        let Some(name) = account else {
            // TODO: global/default-account routing for an unnamed broker plan —
            // mirrors the HTTP receiver's `AccountResolveError::Required` stance.
            // Until a default account exists, an unnamed plan can't be routed and
            // is skipped (logged) rather than guessing credentials.
            tracing::warn!(
                "cron: plan has no named account — skipping (no default-account routing yet)"
            );
            return None;
        };
        match self.state.accounts.get(name).await {
            Ok(meta) => Some(meta),
            Err(err) => {
                tracing::error!("cron: account '{name}' metadata lookup failed: {err}");
                None
            }
        }
    }
}

impl CronEnv for NativeCronEnv {
    async fn acquire_broker(&self, account: Option<&str>) -> Option<BrokerHandle> {
        let meta = self.resolve_meta(account).await?;
        match meta.broker {
            BrokerKind::Oanda => match acquire_oanda(&meta, &self.state.secrets) {
                Ok(b) => Some(BrokerHandle::Oanda(b)),
                Err(err) => {
                    tracing::error!("cron: oanda acquire failed for '{}': {err}", meta.name);
                    None
                }
            },
            BrokerKind::TradeNation => match acquire_tn(&meta).await {
                Ok(b) => Some(BrokerHandle::TradeNation(b)),
                Err(err) => {
                    tracing::error!(
                        "cron: tradenation acquire failed for '{}': {err}",
                        meta.name
                    );
                    None
                }
            },
        }
    }

    async fn dispatch_config(&self, verified: &Verified) -> DispatchConfig {
        // Resolve the per-account caps from metadata; fall back to default caps
        // (all `None` — no narrowing) when the account can't be resolved, so an
        // enter still gets the worker-wide caps rather than failing the config
        // build. The broker acquisition above already logged + skipped a truly
        // unroutable plan, so reaching here with an unresolvable account is rare.
        let caps = self
            .resolve_meta(verified.intent.account.as_deref())
            .await
            .map(|m| m.caps)
            .unwrap_or_default();
        build_dispatch_config_native(&self.state.secrets, &verified.intent.instrument, caps)
    }

    fn record_tick(&self, bundle: TickBundle) {
        // STUB (Task #6): the native runtime will insert the tick bundle into the
        // `tick_bundles` Postgres table here. For now we drop it after a debug
        // line so the recording wiring is visibly pending, not silently missing.
        // TODO(Task #6): insert `bundle` into the `tick_bundles` table.
        tracing::debug!(
            "cron: would record tick bundle trade_id={} (native recording is Task #6, dropping)",
            bundle.correlation_id
        );
    }

    fn signing_key(&self) -> Option<Vec<u8>> {
        // Decoded once at boot and held on `AppState` (the same key the HTTP
        // receiver verifies with). Always present here — `main` aborts if
        // `SIGNING_KEY` is missing/invalid — but the seam is `Option` for the
        // wasm side, so clone the bytes.
        Some(self.state.signing_key.clone())
    }
}
