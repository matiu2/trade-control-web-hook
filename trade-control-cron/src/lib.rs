//! `trade-control-cron` — the shared, worker-free cron engine.
//!
//! Holds the genericized cron **engine tick** so it can be driven by **both**
//! runtimes: the wasm Cloudflare Worker (`trade-control-web-hook`, via its
//! `scheduled()` handler wrapping `&Env` in `EnvCronEnv`) and the native VM
//! worker (`trade-control-worker`, via a tokio-interval scheduler wrapping
//! Postgres + `Secrets` in `NativeCronEnv`).
//!
//! It is deliberately **`worker`-free and wasm-safe**: it compiles for both
//! `wasm32-unknown-unknown` (so the wasm worker can link it) and native. The
//! backend-specific operations (broker acquisition, dispatch-config resolution,
//! tick recording) are hidden behind the [`CronEnv`] seam, whose concrete impls
//! live with their runtimes — never here.
//!
//! The engine tick, the break-even watcher, the daily market-hours blackout
//! refresh, the **order sweep** (`sweep_pending_orders`), and the
//! **spread-blackout cluster** (NY-close apply + cancel + recovery watcher +
//! restore) have moved here. The apply/cancel/restore jobs re-verify a *stored*
//! signed body, so they use the [`CronEnv::signing_key`] seam — the same key the
//! HTTP path verifies with. Only `session_refresh` stays wasm-only: it pre-warms
//! the KV session cache, an optimization the native runtime doesn't need (it
//! re-logins on demand via the broker factory), so it has no native equivalent
//! by design — a deliberate divergence, not a missing port.

mod blackout_apply;
mod blackout_watch;
mod breakeven_watch;
mod broker_handle;
mod constants;
mod engine;
mod seam;
mod spread_lifecycle;
mod sweep;

pub use broker_handle::BrokerHandle;
pub use engine::run_engine_tick;
pub use seam::CronEnv;

pub use blackout_apply::{apply_if_ny_close_edge, widen_open_stops_for_spread_hours};
pub use blackout_watch::watch_recovery;
pub use breakeven_watch::watch as breakeven_watch;
pub use sweep::sweep_pending_orders;
