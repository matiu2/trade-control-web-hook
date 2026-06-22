//! Fire-and-forget recording of engine **tick-bundles** to R2.
//!
//! The cron engine's `tick_one` builds a
//! [`TickBundle`](trade_control_core::tick_bundle::TickBundle) — the full replay
//! tuple for one `(tick, plan)` plus its golden output — and hands it here. We
//! write it to the same R2 bucket the webhook recorder uses
//! ([`R2_BINDING`](crate::recording::R2_BINDING)), under the distinct `ticks/`
//! prefix (see [`TickBundle::r2_key`]), so the downstream `req/`-reader never
//! trips on a tick-bundle.
//!
//! This is the cron-side twin of [`record_to_r2`](crate::recording::record_to_r2):
//! same fail-soft-on-every-axis contract (missing bucket binding, serialize
//! error, or failed put are each logged and swallowed — recording must never
//! break trading), same `wait_until` off the critical path. The only difference
//! is the context type — the cron handler holds a [`worker::ScheduleContext`],
//! not the webhook's [`worker::Context`] — and that the bundle owns its own key.

#[cfg(target_arch = "wasm32")]
use trade_control_core::tick_bundle::TickBundle;

/// Write a tick-bundle to R2 asynchronously via `ctx.wait_until`, so it adds no
/// latency to the cron tick. **Fail-soft on every axis** — a missing bucket
/// binding, a serialize error, or a failed put are logged and swallowed.
#[cfg(target_arch = "wasm32")]
pub fn record_tick_to_r2(env: &worker::Env, ctx: &worker::ScheduleContext, bundle: TickBundle) {
    let bucket = match env.bucket(crate::recording::R2_BINDING) {
        Ok(b) => b,
        Err(_) => {
            rlog!(
                "tick recording: no {} bucket bound — skipped",
                crate::recording::R2_BINDING
            );
            return;
        }
    };
    let key = bundle.r2_key();
    let json = match serde_json::to_string(&bundle) {
        Ok(j) => j,
        Err(err) => {
            rlog_err!("tick recording: serialize failed: {err}");
            return;
        }
    };
    rlog!(
        "tick recording: scheduling R2 put key={key} bytes={}",
        json.len()
    );
    ctx.wait_until(async move {
        match bucket.put(key.clone(), json).execute().await {
            Ok(_) => rlog!("tick recording: R2 put OK key={key}"),
            Err(err) => rlog_err!("tick recording: R2 put failed key={key}: {err}"),
        }
    });
}

/// Native stub so the crate builds and tests run off-wasm. Drops the bundle
/// (no R2 in native tests).
#[cfg(not(target_arch = "wasm32"))]
pub fn record_tick_to_r2(
    _env: &worker::Env,
    _ctx: &worker::ScheduleContext,
    _bundle: trade_control_core::tick_bundle::TickBundle,
) {
}
