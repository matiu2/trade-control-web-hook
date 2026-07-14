//! The native tokio-interval scheduler — the VM replacement for Cloudflare's
//! cron trigger.
//!
//! # One long-lived interval per job — NOT a sleep-per-iteration loop
//!
//! This scheduler is **a small, fixed number of long-lived
//! [`tokio::time::interval`] timers** (one per cron job — currently the engine
//! tick, the break-even watcher, the daily market-hours blackout refresh, the
//! spread-recovery watcher, the NY-close-edge spread-blackout apply, the order
//! sweep, and the Postgres expired-row GC), each re-arming itself via
//! `.tick().await`. It is emphatically **not** a per-plan / per-request timer
//! fan-out.
//!
//! `session_refresh` has no loop here on purpose: it is a wasm-only KV
//! session-cache pre-warm; the native runtime re-logins on demand via the broker
//! factory, so it has no native scheduler job by design (a deliberate
//! divergence, not a missing port). The expired-row GC
//! ([`PgStateStore::gc_expired`](crate::PgStateStore::gc_expired)) is the inverse
//! — a *native-only* job with no wasm equivalent, since KV evicts expired rows
//! automatically.
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

use trade_control_cron::{
    apply_if_ny_close_edge, breakeven_watch, refresh_market_hours_if_due, run_engine_tick,
    sweep_pending_orders, watch_recovery, widen_open_stops_for_spread_hours,
};

use crate::SchedulerConfig;
use crate::http::AppState;
use crate::native_cron::NativeCronEnv;

/// Start the scheduler on a dedicated current-thread + [`LocalSet`] background
/// thread. Returns immediately; the thread runs every cron-job interval for the
/// process lifetime.
///
/// `state` is the shared [`AppState`] (Postgres pool + secrets), `intervals`
/// supplies each job's period: the engine tick
/// ([`SchedulerConfig::engine_interval`]), the break-even watcher + order sweep +
/// spread-recovery watcher + NY-close apply (the frequent
/// [`SchedulerConfig::upkeep_interval`]), the daily market-hours blackout refresh
/// (the self-gating [`SchedulerConfig::daily_tick_interval`]), and the expired-row
/// GC (the hourly [`SchedulerConfig::expiry_sweep_interval`]).
///
/// All loops are joined on one [`LocalSet`], so they share the single
/// current-thread runtime — a slow tick on one job yields cooperatively to the
/// others rather than blocking them. The spread-blackout NY-close apply and its
/// recovery watcher are wired here too (they re-verify a stored signed body via
/// the [`CronEnv::signing_key`](trade_control_cron::CronEnv::signing_key) seam);
/// `blackout_restore` / `blackout_cancel` are *called by* the watcher/apply, not
/// scheduled directly, so they get no interval of their own. The expired-row GC
/// is native-only (KV evicts expired rows for free) and so takes the
/// [`PgStateStore`](crate::PgStateStore) directly, not the `CronEnv` seam.
pub fn run_scheduler(state: Arc<AppState>, intervals: SchedulerConfig) {
    let cron = NativeCronEnv::new(state.clone());
    let engine_period = intervals.engine_interval();
    let breakeven_period = intervals.upkeep_interval();
    let blackout_hours_period = intervals.daily_tick_interval();
    // The spread-recovery watcher runs at the frequent upkeep cadence; the
    // NY-close apply also ticks at upkeep cadence and self-gates internally on
    // `is_ny_close_edge`, mirroring the wasm worker's `now.minute() < 15` wake.
    let blackout_watch_period = intervals.upkeep_interval();
    let blackout_apply_period = intervals.upkeep_interval();
    // The order sweep runs at the frequent upkeep cadence (the wasm worker ran it
    // on every 15-min cron tick). The expired-row GC is the native-only TTL
    // housekeeping, hourly by default.
    let sweep_period = intervals.upkeep_interval();
    let expiry_gc_period = intervals.expiry_sweep_interval();

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
                    blackout_hours_loop(state.clone(), cron.clone(), blackout_hours_period),
                    blackout_watch_loop(state.clone(), cron.clone(), blackout_watch_period),
                    blackout_apply_loop(state.clone(), cron.clone(), blackout_apply_period),
                    sweep_loop(state.clone(), cron.clone(), sweep_period),
                    expiry_gc_loop(state, expiry_gc_period),
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

/// Run one cron-job iteration on an isolated `LocalSet` task so a **panic**
/// inside it is caught (via the task's `JoinHandle`) instead of unwinding the
/// shared `tc-scheduler` thread.
///
/// All cron loops run under one `tokio::join!` on one current-thread runtime in
/// one thread. Awaiting a job future *directly* means a single panic (e.g. an
/// `.expect()` deep in the engine tick against one bad plan) unwinds the whole
/// thread and silently kills EVERY cron job — the axum HTTP thread survives, so
/// the outage is invisible (staging incident 2026-07-14 04:32Z: a
/// `retest_tolerance` ATR panic froze all plan watermarks for ~17h). Spawning
/// the job as a `spawn_local` task contains the unwind to that task; the
/// `JoinError` is logged and the loop's next interval tick proceeds normally.
///
/// `job` is a labelled name for the log line; `fut` is the iteration's future.
async fn run_isolated<F>(job: &str, fut: F)
where
    F: std::future::Future<Output = ()> + 'static,
{
    match tokio::task::spawn_local(fut).await {
        Ok(()) => {}
        Err(err) if err.is_panic() => {
            // The panic payload is usually a `&str`/`String`; surface whatever we
            // can so a genuinely-broken tick is visible rather than a silent gap.
            let payload = err.into_panic();
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            tracing::error!(
                "scheduler: cron job '{job}' PANICKED (isolated, loop continues): {msg}"
            );
        }
        Err(err) => {
            // Cancelled — only happens on runtime shutdown; nothing to recover.
            tracing::warn!("scheduler: cron job '{job}' task ended abnormally: {err}");
        }
    }
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
        // The tick is fail-soft per plan for *errors* (logs + skips a single
        // plan's `Err`), but NOT for panics — `tick_one` returns `Result` and a
        // panic unwinds straight past it. `run_isolated` contains any panic to
        // this one task so a single bad plan can't kill the whole scheduler
        // thread (and with it every other cron job). Clone the cheaply-cloneable
        // handles so the spawned task owns its inputs (`'static`).
        let (state, cron) = (state.clone(), cron.clone());
        run_isolated("engine_tick", async move {
            run_engine_tick(&state.store, &cron, now).await;
        })
        .await;
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

/// The spread-recovery watcher: every `period` (the frequent upkeep cadence)
/// walk every per-trade spread-blackout record and clear it once the spread has
/// recovered (or the backstop fires), restoring widened stops + re-driving
/// cancelled resting orders before the clear. Fail-soft per record (logs +
/// skips); single re-arming interval.
async fn blackout_watch_loop(state: Arc<AppState>, cron: NativeCronEnv, period: Duration) {
    let mut interval = skip_interval(period);

    tracing::info!(
        "scheduler: spread-recovery watch every {}s",
        period.as_secs()
    );

    loop {
        interval.tick().await;
        let now = chrono::Utc::now();
        watch_recovery(&state.store, &cron, now).await;
    }
}

/// The NY-close-edge spread-blackout apply: ticks at the upkeep cadence and
/// **self-gates on `is_ny_close_edge`** inside
/// [`apply_if_ny_close_edge`] — most ticks no-op. The interval is just the wake
/// cadence (mirroring the wasm worker's `now.minute() < 15` wake), so a tick
/// faster than the close hour costs nothing but the edge check. When the edge
/// hits it opens the blackout window, widens open stops, and cancels resting
/// orders. Fail-soft per position/order; single re-arming interval.
async fn blackout_apply_loop(state: Arc<AppState>, cron: NativeCronEnv, period: Duration) {
    let mut interval = skip_interval(period);

    tracing::info!(
        "scheduler: NY-close-edge blackout apply wake every {}s (self-gates on the close edge)",
        period.as_secs()
    );

    loop {
        interval.tick().await;
        let now = chrono::Utc::now();
        // System 2 — per-instrument spread-hour widen, every tick (self-gates
        // per-instrument on the baked mask). System 1 window + System 3 cancel
        // stay NY-close-edge-gated inside `apply_if_ny_close_edge`.
        widen_open_stops_for_spread_hours(&state.store, &cron, now).await;
        apply_if_ny_close_edge(&state.store, &cron, now).await;
    }
}

/// The order sweep: every `period` (the frequent upkeep cadence) walk every
/// tracked pending `EntryAttempt` and cancel + delete any whose alert window
/// expired, whose bar-expiry fired, that's caught in a market-hours blackout, or
/// whose SL has been overtaken by current price. Fail-soft per attempt (logs +
/// skips); single re-arming interval. Shared with the wasm worker via the
/// `trade-control-cron` crate.
async fn sweep_loop(state: Arc<AppState>, cron: NativeCronEnv, period: Duration) {
    let mut interval = skip_interval(period);

    tracing::info!("scheduler: order sweep every {}s", period.as_secs());

    loop {
        interval.tick().await;
        let now = chrono::Utc::now();
        sweep_pending_orders(&state.store, &cron, now).await;
    }
}

/// The expired-row GC: every `period` (hourly by default) physically delete TTL
/// rows past `expires_at` across every TTL table — the native stand-in for KV's
/// automatic eviction. Native-only (the wasm KV store evicts for free), so it
/// takes the [`PgStateStore`](crate::PgStateStore) directly rather than the
/// `CronEnv` seam. Fail-soft: a failed GC pass is logged and the loop continues
/// to the next tick (reads already filter expired rows, so a missed pass is
/// harmless). Single re-arming interval.
async fn expiry_gc_loop(state: Arc<AppState>, period: Duration) {
    let mut interval = skip_interval(period);

    tracing::info!("scheduler: expired-row GC every {}s", period.as_secs());

    loop {
        interval.tick().await;
        match state.store.gc_expired().await {
            Ok(deleted) => tracing::info!("scheduler: expired-row GC deleted {deleted} row(s)"),
            Err(err) => tracing::error!("scheduler: expired-row GC failed: {err}"),
        }
    }
}
