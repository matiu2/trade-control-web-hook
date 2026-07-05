//! The wasm worker's [`CronEnv`] implementation.
//!
//! The [`CronEnv`] **trait** (and [`BrokerHandle`]) moved to the shared
//! `trade-control-cron` crate so the engine tick is worker-free and runs on
//! both runtimes. What stays here is the wasm-specific impl, [`EnvCronEnv`],
//! which references Cloudflare's `worker::Env` — the one piece that can't be
//! shared. (The native runtime has its own `NativeCronEnv`.)
//!
//! [`EnvCronEnv`] delegates each seam method to the existing `Env`-backed
//! helpers, so the Cloudflare `scheduled()` path runs the now-shared engine
//! unchanged.

use trade_control_core::dispatch_config::DispatchConfig;
use trade_control_core::incoming::Verified;
use trade_control_core::tick_bundle::TickBundle;
use trade_control_cron::{BrokerHandle, CronEnv};

/// wasm worker [`CronEnv`]: delegates each seam method to the existing
/// `Env`-backed helpers, so the Cloudflare `scheduled()` path runs the
/// now-shared engine unchanged.
///
/// Holds the cron's `&Env` (broker acquisition + dispatch-config) and
/// `&ScheduleContext` (the `wait_until` handle the R2 tick-write needs). Not
/// gated to wasm: the `scheduled()` body that builds it compiles on the native
/// test target too, and the three delegated helpers each carry their own native
/// stub (broker acquisition returns `None`, tick recording drops the bundle).
pub(crate) struct EnvCronEnv<'a> {
    pub env: &'a worker::Env,
    pub ctx: &'a worker::ScheduleContext,
}

impl CronEnv for EnvCronEnv<'_> {
    async fn acquire_broker(&self, account: Option<&str>) -> Option<BrokerHandle> {
        super::sweep::acquire_broker_for_account(self.env, account).await
    }

    async fn dispatch_config(&self, verified: &Verified) -> DispatchConfig {
        crate::build_dispatch_config(self.env, verified).await
    }

    fn record_tick(&self, bundle: TickBundle) {
        crate::tick_recording::record_tick_to_r2(self.env, self.ctx, bundle);
    }

    fn signing_key(&self) -> Option<Vec<u8>> {
        crate::signing_key(self.env)
    }
}
