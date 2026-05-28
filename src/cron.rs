//! Scheduled (cron) entry-point for the worker. Periodic upkeep that
//! the request-response `fetch` handler can't do: sweep pending
//! broker orders, cancel ones whose SL has been breached, cancel
//! expired ones.
//!
//! Triggered by Cloudflare cron triggers declared in `wrangler.toml`
//! — see the `[[triggers.crons]]` table.

mod constants;
mod sweep;

use worker::{Env, ScheduleContext, ScheduledEvent, event};

/// Cron entry-point. Cloudflare invokes this on every schedule
/// declared under `[[triggers.crons]]` in `wrangler.toml`. The handler
/// owns one sweep per fire — no per-cron dispatch here yet since
/// there's only one scheduled job. Add a `match event.cron()` if a
/// second job lands.
#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    crate::tracing_console::ConsoleSubscriber::install();
    let now = chrono::Utc::now();
    sweep::sweep_pending_orders(&env, now).await;
}
