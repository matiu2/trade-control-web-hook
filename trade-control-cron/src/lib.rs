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
//! refresh, and the **spread-blackout cluster** (NY-close apply + cancel +
//! recovery watcher + restore) have moved here. The apply/cancel/restore jobs
//! re-verify a *stored* signed body, so they use the [`CronEnv::signing_key`]
//! seam — the same key the HTTP path verifies with. The order sweep / session
//! refresh cron jobs still live in the wasm worker (they run on the raw `&Env`
//! path and aren't ported yet).

mod blackout_apply;
mod blackout_cancel;
mod blackout_hours;
mod blackout_restore;
mod blackout_watch;
mod breakeven_watch;
mod broker_handle;
mod constants;
mod engine;
mod seam;

pub use broker_handle::BrokerHandle;
pub use engine::run_engine_tick;
pub use seam::CronEnv;

pub use blackout_apply::apply_if_ny_close_edge;
pub use blackout_cancel::cancel_resting_orders;
pub use blackout_hours::refresh_if_due as refresh_market_hours_if_due;
pub use blackout_restore::restore_cancelled_orders;
pub use blackout_watch::watch_recovery;
pub use breakeven_watch::watch as breakeven_watch;
