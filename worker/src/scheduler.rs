//! The native tokio-interval scheduler — the VM replacement for Cloudflare's
//! cron trigger.
//!
//! # One long-lived interval per job — NOT a sleep-per-iteration loop
//!
//! This scheduler is **a small, fixed number of long-lived
//! [`tokio::time::interval`] timers** (one per cron job — currently just the
//! engine tick), each re-arming itself via `.tick().await`. It is emphatically
//! **not** a per-plan / per-request timer fan-out.
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

use trade_control_cron::run_engine_tick;

use crate::SchedulerConfig;
use crate::http::AppState;
use crate::native_cron::NativeCronEnv;

/// Start the scheduler on a dedicated current-thread + [`LocalSet`] background
/// thread. Returns immediately; the thread runs the engine-tick interval for the
/// process lifetime.
///
/// `state` is the shared [`AppState`] (Postgres pool + secrets), `intervals`
/// supplies the engine-tick period via [`SchedulerConfig::engine_interval`].
pub fn run_scheduler(state: Arc<AppState>, intervals: SchedulerConfig) {
    let cron = NativeCronEnv::new(state.clone());
    let engine_period = intervals.engine_interval();

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
                engine_tick_loop(state, cron, engine_period).await;
            });
        })
        .map(|_| ())
        .unwrap_or_else(|e| tracing::error!("failed to spawn scheduler thread: {e}"));
}

/// The engine-tick job: one long-lived re-arming [`tokio::time::interval`] that
/// runs [`run_engine_tick`] every `period`. See the module docs for why this is
/// a single interval (not a sleep-per-iteration loop) and why missed ticks are
/// skipped rather than caught up.
async fn engine_tick_loop(state: Arc<AppState>, cron: NativeCronEnv, period: Duration) {
    let mut interval = tokio::time::interval(period);
    // A slow tick must not queue catch-up ticks — re-align to the next boundary.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
