//! Scheduled (cron) entry-point for the worker. Periodic upkeep that
//! the request-response `fetch` handler can't do: sweep pending
//! broker orders, cancel ones whose SL has been breached, cancel
//! expired ones, and run the spread-blackout state machine.
//!
//! Triggered by Cloudflare cron triggers declared in `wrangler.toml`
//! — see the `[triggers]` `crons` array. Two jobs share this handler:
//!
//! * **Cron 1 (daily, `5 21`/`5 22 * * *`)** — the NY-close-edge check.
//!   When `is_ny_close_edge(now)`, open the global spread-blackout
//!   window marker (`blackout_apply`). The two candidate minutes cover
//!   the EDT (21:00 UTC) and EST (22:00 UTC) seasons; the wrong-season
//!   fire no-ops in Rust via `is_ny_close_edge`.
//! * **Cron 2 (every 15 min, `*/15 * * * *`)** — the existing session
//!   refresh + order sweep, plus the spread-recovery watcher
//!   (`blackout_watch`).

mod blackout_apply;
mod blackout_watch;
mod constants;
pub(crate) mod session_meta;
mod session_refresh;
mod sweep;

use worker::{Env, ScheduleContext, ScheduledEvent, event};

/// Cron entry-point. Cloudflare invokes this on every schedule declared
/// under `[triggers]` in `wrangler.toml`, passing back the matched cron
/// expression. We dispatch on `event.cron()` so the rare daily
/// NY-close-edge job and the frequent 15-min sweep don't share a body.
#[event(scheduled)]
pub async fn scheduled(event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    crate::tracing_console::ConsoleSubscriber::install();
    let now = chrono::Utc::now();
    // Match the *exact* daily candidate exprs so a fat-fingered new cron
    // later doesn't silently fall into the high-frequency 15-min arm.
    match event.cron().as_str() {
        "5 21 * * *" | "5 22 * * *" => {
            blackout_apply::apply_if_ny_close_edge(&env, now).await;
        }
        _ => {
            session_refresh::refresh_stale_sessions(&env, now, constants::STALE_AFTER).await;
            sweep::sweep_pending_orders(&env, now).await;
            blackout_watch::watch_recovery(&env, now).await;
        }
    }
}
