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

mod blackout_apply;
mod blackout_cancel;
mod blackout_hours;
mod blackout_restore;
mod blackout_watch;
mod blackout_widen;
mod breakeven_watch;
mod constants;
mod engine;
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

    // Frequent upkeep — every 15-min tick.
    session_refresh::refresh_stale_sessions(&env, now, constants::STALE_AFTER).await;
    sweep::sweep_pending_orders(&env, now).await;
    blackout_watch::watch_recovery(&env, now).await;
    // Break-even stop management — move open positions' stops to entry once a
    // candle closes past 50%-to-TP (BUG-replay-no-breakeven-stop-at-50pct).
    breakeven_watch::watch(&env, now).await;

    // Server-side trade-plan engine — evaluate every registered plan against
    // fresh broker candles and dispatch fired intents. Runs in parallel with
    // the webhook (no self-gate); the `*/15` schedule stays — the `*/1`–`*/5`
    // bump is Stage F, once the engine is proven on demo.
    engine::run_engine_tick(&env, &ctx, now).await;

    // NY-close-edge job — fire exactly once per close hour, on the :00
    // tick. `apply_if_ny_close_edge` re-checks `is_ny_close_edge` itself,
    // so the minute gate is purely to avoid running it on all four ticks
    // of the close hour.
    if now.minute() < 15 {
        blackout_apply::apply_if_ny_close_edge(&env, now).await;
        // Daily market-hours blackout refresh — self-gates on its own hour
        // (06:00 UTC). Resolves each traded instrument's current-season
        // session into UTC no-entry windows and writes them to KV.
        blackout_hours::refresh_if_due(&env, now).await;
    }
}
