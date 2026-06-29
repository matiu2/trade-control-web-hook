//! The native tokio-interval scheduler — the VM replacement for Cloudflare's
//! cron trigger.
//!
//! # One long-lived interval per job — NOT a sleep-per-iteration loop
//!
//! This scheduler is **a small, fixed number of long-lived
//! [`tokio::time::interval`] timers** (one per cron job — currently the engine
//! tick, the break-even watcher, and the daily market-hours blackout refresh),
//! each re-arming itself via `.tick().await`. It is emphatically **not** a
//! per-plan / per-request timer fan-out.
//!
//! That distinction is load-bearing. tokio's timer driver guards its wheel with
//! a single mutex; under a flood of short-lived `Sleep` timers being created and
//! dropped (the classic `loop { sleep(period).await; work().await }` shape, or a
//! timer-per-entity design) that mutex becomes a contention hot-spot
//! (tokio#6504). We stay in the safe regime by construction:
//!
//! * **Re-arming intervals, not fresh sleeps.** `tokio::time::interval(period)`
//!   allocates one timer entry and reuses it every `.tick()`. We never write
//!   `sleep(period).await` in a loop, which would churn a new `Sleep` each pass.
//! * **`MissedTickBehavior::Skip`.** A slow tick (a broker fetch stall) must not
//!   queue catch-up ticks — that's both wrong trading semantics (we want the
//!   *next* scheduled bar, not a burst of stale ones) and it would defeat the
//!   single-timer property. `Skip` re-aligns to the next period boundary.
//! * **A handful of timers, period.** New cron jobs add one interval each; we do
//!   not — and must not — spin up a timer per trade plan. If a future change is
//!   tempted to give each plan its own timer, that's exactly the fan-out
//!   tokio#6504 warns about: keep the evaluation inside the single engine tick,
//!   which already walks every registered plan in one pass.
//!
//! # Which runtime it runs on
//!
//! The engine tick drives the broker SDKs, whose futures are `?Send` (single-
//! threaded clients) — same constraint as the HTTP dispatcher. So the scheduler
//! gets its **own dedicated current-thread runtime + [`LocalSet`]** on a
//! background thread, mirroring [`crate::http::Dispatcher`]. A dedicated thread
//! (rather than sharing the HTTP dispatcher's single-flight loop) keeps a slow
//! engine tick from blocking inbound request processing, and vice-versa.
//!
//! # Shutdown
//!
//! The scheduler thread is a detached background thread owned by the process.
//! On `main`'s graceful-shutdown signal the process exits and the thread is torn
//! down with it; the in-flight tick (if any) is abandoned mid-flight, which is
//! safe — the engine persists plan state *before* dispatching, and every tick is
//! a fresh pure function of `(store, now)`, so the next process start simply
//! re-evaluates from the persisted watermark. A cleaner cooperative abort (a
//! shutdown channel) is a nice-to-have, not required for this increment.

use std::sync::Arc;
use std::time::Duration;

use trade_control_cron::{breakeven_watch, refresh_market_hours_if_due, run_engine_tick};

use crate::SchedulerConfig;
use crate::http::AppState;
use crate::native_cron::NativeCronEnv;

/// Start the scheduler on a dedicated current-thread + [`LocalSet`] background
/// thread. Returns immediately; the thread runs every cron-job interval for the
/// process lifetime.
///
/// `state` is the shared [`AppState`] (Postgres pool + secrets), `intervals`
/// supplies each job's period: the engine tick
/// ([`SchedulerConfig::engine_interval`]), the break-even watcher (the frequent
/// [`SchedulerConfig::upkeep_interval`]), and the daily market-hours blackout
/// refresh (the self-gating [`SchedulerConfig::daily_tick_interval`]).
///
/// The three loops are joined on one [`LocalSet`], so they share the single
/// current-thread runtime — a slow tick on one job yields cooperatively to the
/// others rather than blocking them. (Two more cron jobs — the spread-blackout
/// NY-close apply and its recovery watcher — are *not* wired here: they still
/// live in the wasm worker because they need an HMAC signing-key seam the
/// [`CronEnv`](trade_control_cron::CronEnv) trait doesn't expose; see the cron
/// crate's `lib.rs` note.)
pub fn run_scheduler(state: Arc<AppState>, intervals: SchedulerConfig) {
    let cron = NativeCronEnv::new(state.clone());
    let engine_period = intervals.engine_interval();
    let breakeven_period = intervals.upkeep_interval();
    let blackout_hours_period = intervals.daily_tick_interval();

    std::thread::Builder::new()
        .name("tc-scheduler".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("scheduler runtime build failed: {e}");
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                // All cron loops run forever on the one current-thread runtime;
                // `join!` drives them concurrently and never returns.
                tokio::join!(
                    engine_tick_loop(state.clone(), cron.clone(), engine_period),
                    breakeven_loop(state.clone(), cron.clone(), breakeven_period),
                    blackout_hours_loop(state, cron, blackout_hours_period),
                );
            });
        })
        .map(|_| ())
        .unwrap_or_else(|e| tracing::error!("failed to spawn scheduler thread: {e}"));
}

/// Build a re-arming [`tokio::time::interval`] with the catch-up-suppressing
/// [`MissedTickBehavior::Skip`](tokio::time::MissedTickBehavior::Skip). Shared by
/// every cron loop so they all get the same single-timer, no-burst semantics
/// (see the module docs for why this matters).
fn skip_interval(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(period);
    // A slow tick must not queue catch-up ticks — re-align to the next boundary.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

/// The engine-tick job: one long-lived re-arming [`tokio::time::interval`] that
/// runs [`run_engine_tick`] every `period`. See the module docs for why this is
/// a single interval (not a sleep-per-iteration loop) and why missed ticks are
/// skipped rather than caught up.
async fn engine_tick_loop(state: Arc<AppState>, cron: NativeCronEnv, period: Duration) {
    let mut interval = skip_interval(period);

    tracing::info!("scheduler: engine tick every {}s", period.as_secs());

    loop {
        interval.tick().await;
        let now = chrono::Utc::now();
        // The tick is fail-soft per plan (it logs + skips a single plan's
        // failure), so a panic here would be a bug, not an expected path — but it
        // would still take down only this scheduler thread, never the HTTP
        // receiver (separate thread + runtime).
        run_engine_tick(&state.store, &cron, now).await;
    }
}

/// The break-even-watch job: every `period` (the frequent upkeep cadence) move
/// each eligible open position's stop to break-even once a candle has closed past
/// 50%-to-TP. Fail-soft per position (logs + skips), single re-arming interval.
async fn breakeven_loop(state: Arc<AppState>, cron: NativeCronEnv, period: Duration) {
    let mut interval = skip_interval(period);

    tracing::info!("scheduler: breakeven watch every {}s", period.as_secs());

    loop {
        interval.tick().await;
        let now = chrono::Utc::now();
        breakeven_watch(&state.store, &cron, now).await;
    }
}

/// The daily market-hours blackout refresh: ticks at the daily cadence and
/// **self-gates on the 06:00 UTC hour** inside
/// [`refresh_market_hours_if_due`] — most ticks no-op. The interval is just the
/// wake cadence (mirroring the wasm worker's `now.minute() < 15` wake), so a tick
/// faster than once an hour costs nothing but the hour check. Fail-open per
/// instrument; single re-arming interval.
async fn blackout_hours_loop(state: Arc<AppState>, cron: NativeCronEnv, period: Duration) {
    let mut interval = skip_interval(period);

    tracing::info!(
        "scheduler: market-hours blackout refresh wake every {}s (self-gates on 06:00 UTC)",
        period.as_secs()
    );

    loop {
        interval.tick().await;
        let now = chrono::Utc::now();
        refresh_market_hours_if_due(&state.store, &cron, now).await;
    }
}
