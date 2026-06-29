//! The cron `&Env` seam — one trait that lets the engine tick run on **both**
//! the wasm worker (Cloudflare `&Env` + `KvStateStore`) and the native runtime
//! (`PgStateStore` + the native broker factory), without the engine logic
//! forking.
//!
//! The engine's `&Env` hid exactly three backend-specific operations:
//!
//! 1. **Broker acquisition** — pick a broker for a plan's account. On wasm
//!    that's the KV account index + `Env` secrets; on native it's the Postgres
//!    metadata store + `Secrets`.
//! 2. **Dispatch-config resolution** — pre-resolve the per-enter
//!    [`DispatchConfig`] (risk caps, pip fallback, per-account caps) at the
//!    edge, so the dispatch core stays backend-free. wasm reads `Env` + the KV
//!    account index; native reads `Secrets` + Postgres.
//! 3. **Tick recording** — write the per-`(tick, plan)` [`TickBundle`] to a
//!    sink. wasm writes R2; native writes Postgres (Task #6 — a drop stub for
//!    now).
//!
//! Rather than three separate generic params threaded through every engine
//! function, these travel together as one bundled trait, [`CronEnv`]: the
//! engine took a single `&Env`, so it now takes a single `&impl CronEnv`. The
//! trait is used **generically** (`<C: CronEnv>`), never as `dyn` — matching
//! how the HTTP dispatch is generic over its `Broker`, not boxed — so the
//! `?Send` broker futures and `async fn in trait` cause no object-safety
//! trouble. (Both schedulers run the cron on a local-thread runtime, so no
//! `Send` bounds are needed or wanted.)
//!
//! The concrete impls live with their runtimes: `EnvCronEnv` in the wasm worker
//! (it references `worker::Env`), `NativeCronEnv` in the native worker (it
//! references `PgMetadataStore` + `Secrets`). Only the **trait** lives here.
//!
//! [`BrokerHandle`] — the broker enum the engine matches on — lives in
//! [`crate::broker_handle`]; it holds the same `OandaBroker` /
//! `TradeNationAdapter` types on both runtimes, so it is shared verbatim.

use trade_control_core::dispatch_config::DispatchConfig;
use trade_control_core::incoming::Verified;
use trade_control_core::tick_bundle::TickBundle;

use crate::broker_handle::BrokerHandle;

/// The backend seam the cron engine threads instead of a raw `&Env`. One trait
/// bundles the three `&Env`-hidden operations (broker acquisition, dispatch-config
/// resolution, tick recording) so the engine takes a single `&impl CronEnv` and
/// the same engine code runs on wasm (`EnvCronEnv`) and native (`NativeCronEnv`).
///
/// Used generically (`<C: CronEnv>`), never boxed — see the module docs for why
/// the `?Send` / `async fn in trait` is fine here.
///
/// `async_fn_in_trait` is allowed deliberately: the broker futures are `?Send`
/// (single-threaded SDK clients), and both schedulers drive the cron on a
/// local-thread runtime, so the absent `Send` bound the lint warns about is
/// exactly what we want — adding `+ Send` would break the design.
#[allow(async_fn_in_trait)]
pub trait CronEnv {
    /// Acquire a broker for a plan's account. `None` → worker-global OANDA (the
    /// fetch-path default); `Some(name)` → the account's broker kind. Returns
    /// `None` when the broker can't be acquired (KV/metadata miss, login
    /// failure); the caller logs and skips the plan for this tick.
    async fn acquire_broker(&self, account: Option<&str>) -> Option<BrokerHandle>;

    /// Resolve the [`DispatchConfig`] for an enter at this edge (risk caps, pip
    /// fallback, per-account caps), so `run_enter` itself stays backend-free.
    async fn dispatch_config(&self, verified: &Verified) -> DispatchConfig;

    /// Record one tick's [`TickBundle`] to the backend sink. Fire-and-forget,
    /// fail-soft — recording must never break trading. wasm writes R2; native
    /// writes Postgres (Task #6).
    fn record_tick(&self, bundle: TickBundle);
}
