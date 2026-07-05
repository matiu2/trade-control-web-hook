//! Scheduled (cron) entry-point for the worker. Periodic upkeep that
//! the request-response `fetch` handler can't do: sweep pending
//! broker orders, cancel ones whose SL has been breached, cancel
//! expired ones, and run the spread-blackout state machine.
//!
//! Triggered by a **single** Cloudflare cron trigger declared in
//! `wrangler.toml` — `*/15 * * * *`. (Cloudflare caps an account at 5 cron
//! triggers across all workers; collapsing to one leaves room for the
//! dev/staging/prod split.) One handler runs every job, each self-gating
//! on `now`:
//!
//! * **Every tick (every 15 min)** — session refresh + order sweep +
//!   the spread-recovery watcher (`blackout_watch`).
//! * **Daily, 06:00 UTC** — refresh each traded instrument's market-hours
//!   blackout windows in KV (`blackout_hours`). Self-gates on the hour;
//!   shares the `now.minute() < 15` once-per-hour guard with `blackout_apply`.
//! * **NY-close edge only** — open the global spread-blackout window
//!   marker (`blackout_apply`). This used to be its own daily cron
//!   (`5 21`/`5 22`); it's now run from the 15-min arm, gated by
//!   `is_ny_close_edge(now)` (which matches the close *hour*, 21:00 UTC
//!   under EDT / 22:00 UTC under EST) **and** `now.minute() < 15` so it
//!   fires exactly once — on the :00 tick of the close hour — preserving
//!   the old once-per-day semantics. `blackout_apply` is also internally
//!   idempotent (window marker + per-record guard), so a double-fire
//!   would be harmless; the minute gate just avoids 4× the broker calls.

mod constants;
mod seam;
pub(crate) mod session_meta;
mod session_refresh;
mod sweep;

use worker::{Env, ScheduleContext, ScheduledEvent, event};

/// Cron entry-point. A single `*/15 * * * *` trigger drives every job;
/// each self-gates on `now` (see module docs). Using `chrono::Timelike`
/// for the minute gate on the NY-close-edge job.
#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, env: Env, ctx: ScheduleContext) {
    use chrono::Timelike;
    crate::tracing_console::ConsoleSubscriber::install();
    let now = chrono::Utc::now();

    // Frequent upkeep — every 15-min tick. `session_refresh` is wasm-only (it
    // pre-warms the KV session cache, an optimization the native runtime doesn't
    // need — it re-logins on demand via the broker factory), so it stays here on
    // the raw `&Env` path with no native equivalent by design.
    session_refresh::refresh_stale_sessions(&env, now, constants::STALE_AFTER).await;

    // Open the KV store + our `CronEnv` impl once and reuse them for every cron
    // job that now lives in the shared `trade-control-cron` crate: the order
    // sweep, the spread-recovery watcher, the break-even watcher, the engine
    // tick, the NY-close-edge apply, and the daily market-hours blackout refresh.
    // Each is generic over its store + backend seam (`CronEnv`) so the *same*
    // code runs on the native runtime (Task #5); here we wrap `&Env`/`&ctx` in
    // `EnvCronEnv` and hand both in, never touching `Env` inside the shared logic.
    if let Some(store) = sweep::open_store(&env) {
        let cron_env = seam::EnvCronEnv {
            env: &env,
            ctx: &ctx,
        };

        // Order sweep — cancel + delete each pending `EntryAttempt` whose alert
        // window expired, whose bar-expiry fired, that's caught in a market-hours
        // blackout, or whose SL has been overtaken. Runs after session_refresh,
        // before the spread-recovery watcher (call order preserved across the
        // port to the shared crate).
        trade_control_cron::sweep_pending_orders(&store, &cron_env, now).await;

        // Spread-recovery watcher — clear each per-trade blackout record once the
        // spread has recovered (or the backstop fires), restoring widened stops +
        // re-driving cancelled resting orders before the clear.
        trade_control_cron::watch_recovery(&store, &cron_env, now).await;

        // Break-even stop management — move open positions' stops to entry once a
        // candle closes past 50%-to-TP (BUG-replay-no-breakeven-stop-at-50pct).
        trade_control_cron::breakeven_watch(&store, &cron_env, now).await;

        // Server-side trade-plan engine — evaluate every registered plan against
        // fresh broker candles and dispatch fired intents. Runs in parallel with
        // the webhook (no self-gate); the `*/15` schedule stays — the `*/1`–`*/5`
        // bump is Stage F, once the engine is proven on demo.
        trade_control_cron::run_engine_tick(&store, &cron_env, now).await;

        // System 2 — per-instrument spread-hour stop-widen. Fires on EVERY
        // tick (not just the NY-close hour) because spread hours are
        // per-instrument and the ~30-min lead straddles the top of the hour;
        // the widen self-gates per-instrument on the baked spread-hour mask,
        // so most ticks no-op cheaply.
        trade_control_cron::widen_open_stops_for_spread_hours(&store, &cron_env, now).await;

        // NY-close-edge job — System 1 (entry-reject window) + System 3
        // (cancel resting orders), fired exactly once per close hour on the
        // :00 tick. `apply_if_ny_close_edge` re-checks `is_ny_close_edge`
        // itself, so the minute gate is purely to avoid running it on all
        // four ticks of the close hour.
        if now.minute() < 15 {
            trade_control_cron::apply_if_ny_close_edge(&store, &cron_env, now).await;
            // Daily market-hours blackout refresh — self-gates on its own hour
            // (06:00 UTC). Resolves each traded instrument's current-season
            // session into UTC no-entry windows and writes them to KV.
            trade_control_cron::refresh_market_hours_if_due(&store, &cron_env, now).await;
        }
    }
}
