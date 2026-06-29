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
//! The engine tick, the break-even watcher, and the daily market-hours blackout
//! refresh have moved here so far. The order sweep / spread-blackout
//! apply+recovery / session cron jobs follow later through this same crate (two
//! of them, `blackout_apply` and the spread-recovery watcher, also need an HMAC
//! signing-key seam the [`CronEnv`] trait doesn't yet expose — see their
//! still-in-worker counterparts).

mod blackout_hours;
mod breakeven_watch;
mod broker_handle;
mod engine;
mod seam;

pub use broker_handle::BrokerHandle;
pub use engine::run_engine_tick;
pub use seam::CronEnv;

pub use blackout_hours::refresh_if_due as refresh_market_hours_if_due;
pub use breakeven_watch::watch as breakeven_watch;
