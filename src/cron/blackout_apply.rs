//! Cron 1 — NY-close-edge handler. Fires from the daily candidate crons
//! (`5 21 * * *` / `5 22 * * *`). Sub-plan 2 scope: when
//! `is_ny_close_edge(now)`, open the global spread-blackout window
//! marker. Widening open stops (Sub-plan 4) and cancelling resting
//! orders (Sub-plan 5) hang off this same edge later.

use chrono::{DateTime, Utc};
use trade_control_core::ny_clock::is_ny_close_edge;
use trade_control_core::state::StateStore;
use worker::{Env, console_error, console_log};

use super::constants::BLACKOUT_BACKSTOP_SECONDS;
use super::sweep::open_store;

/// Open the global spread-blackout window marker iff `now` is the
/// NY-close edge. The two daily crons fire at both DST candidate hours
/// (21:05 EDT, 22:05 EST); `is_ny_close_edge` decides which one is the
/// real edge this season and no-ops the other.
pub async fn apply_if_ny_close_edge(env: &Env, now: DateTime<Utc>) {
    if !is_ny_close_edge(now) {
        console_log!("blackout: cron fired but not NY-close edge ({now}); no-op");
        return;
    }
    let Some(store) = open_store(env) else {
        return;
    };
    // The window TTL keys off the same backstop the recovery watcher
    // uses, so the marker and the per-record backstop can never drift.
    let ttl = BLACKOUT_BACKSTOP_SECONDS;
    match store.set_spread_blackout_window(now, ttl).await {
        Ok(()) => console_log!("blackout: window opened at {now} (ttl {ttl}s)"),
        Err(err) => console_error!("blackout: failed to open window: {err}"),
    }
    // Sub-plan 4 inserts list_open_positions → widen → record here.
    // Sub-plan 5 inserts list_pending_orders → cancel → store-intent here.
}
