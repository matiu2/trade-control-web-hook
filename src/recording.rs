//! Per-request event recording to R2 (the wasm-worker runtime glue).
//!
//! Every inbound request is captured as a single JSON object: the
//! verbatim signed body, the request headers, the final HTTP response
//! (status + outcome), and **every** log line the handler emitted during
//! dispatch. The object is written to R2 asynchronously via
//! `ctx.wait_until` so recording adds no latency to the response.
//!
//! The *pure* record types ([`LogLine`], [`RequestRecord`],
//! [`mint_request_id`], [`ids_from_body`]) now live in
//! [`trade_control_core::recording`] so both runtimes (this wasm worker and
//! the native Postgres runtime) build the *same* record. They are re-exported
//! below so the existing `recording::RequestRecord` etc. call sites in this
//! crate keep resolving. This module retains the worker-coupled pieces: the
//! [`rlog!`]/[`rlog_err!`] macros, the thread-local log buffer, the R2 writer,
//! and the bucket binding name.
//!
//! ## Why a thread-local log buffer
//!
//! Log lines come from two places: the [`rlog!`]/[`rlog_err!`] macros
//! (which replace the worker's direct `console_log!`/`console_error!`
//! calls) and broker-crate `tracing::warn!`/`error!` events routed
//! through [`crate::tracing_console::ConsoleSubscriber`]. To capture both
//! without threading a buffer argument through every function signature,
//! both push into one per-request [`LOG_BUFFER`].
//!
//! Cloudflare Workers run one request to completion on a single thread,
//! so a `thread_local!` is effectively per-request state here. [`begin`]
//! clears it at the start of `main`; [`take_logs`] drains it at the end.
//! (Off-wasm, in `cargo test`, the same buffer backs the macro so the
//! capture logic is unit-testable natively.)

use std::cell::RefCell;

// Re-export the pure record types from core so the in-crate `recording::*`
// references (in `src/lib.rs`, `tick_recording.rs`, etc.) keep resolving.
pub use trade_control_core::recording::{LogLine, RequestRecord, ids_from_body, mint_request_id};

thread_local! {
    static LOG_BUFFER: RefCell<Vec<LogLine>> = const { RefCell::new(Vec::new()) };
}

/// Reset the per-request log buffer. Call once at the top of `main`.
pub fn begin() {
    LOG_BUFFER.with(|b| b.borrow_mut().clear());
}

/// Append a line to the per-request buffer. Used by the [`rlog!`] macros
/// and by [`crate::tracing_console::ConsoleSubscriber`].
pub fn push(level: &'static str, msg: String) {
    LOG_BUFFER.with(|b| b.borrow_mut().push(LogLine { level, msg }));
}

/// Drain the buffer, returning everything captured this request.
pub fn take_logs() -> Vec<LogLine> {
    LOG_BUFFER.with(|b| std::mem::take(&mut *b.borrow_mut()))
}

/// Record-aware replacement for `worker::console_log!`. Forwards to the
/// real console (so `wrangler tail` is unchanged) **and** appends to the
/// per-request buffer. Off-wasm it routes to `tracing::info!` so the line
/// still surfaces in native test output, and still buffers.
///
/// In scope crate-wide via `#[macro_use] mod recording;` in `lib.rs`.
macro_rules! rlog {
    ($($arg:tt)*) => {{
        let __msg = ::std::format!($($arg)*);
        $crate::recording::push("log", __msg.clone());
        #[cfg(target_arch = "wasm32")]
        ::worker::console_log!("{}", __msg);
        #[cfg(not(target_arch = "wasm32"))]
        ::tracing::info!("{}", __msg);
    }};
}

/// Record-aware replacement for `worker::console_error!`. See [`rlog!`].
macro_rules! rlog_err {
    ($($arg:tt)*) => {{
        let __msg = ::std::format!($($arg)*);
        $crate::recording::push("error", __msg.clone());
        #[cfg(target_arch = "wasm32")]
        ::worker::console_error!("{}", __msg);
        #[cfg(not(target_arch = "wasm32"))]
        ::tracing::error!("{}", __msg);
    }};
}

/// R2 bucket binding name. Add `[[r2_buckets]] binding = "TRADE_CONTROL_R2"`
/// to `wrangler.toml` and create the bucket before deploying. If the
/// binding is absent, recording is skipped (fail-soft) — the request
/// still succeeds. Only read by the wasm `record_to_r2`; dead on native.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const R2_BINDING: &str = "TRADE_CONTROL_R2";

/// Write the record to R2 asynchronously (via `ctx.wait_until`), so it
/// adds no latency to the response. **Fail-soft on every axis**: a missing
/// bucket binding, a serialization error, or a failed put are all logged
/// and swallowed — recording must never break trading.
#[cfg(target_arch = "wasm32")]
pub fn record_to_r2(env: &worker::Env, ctx: &worker::Context, record: RequestRecord) {
    let bucket = match env.bucket(R2_BINDING) {
        Ok(b) => b,
        Err(_) => {
            // No binding configured — skip silently-ish. One line so the
            // operator notices if they expected recording to be on.
            worker::console_log!("recording: no {R2_BINDING} bucket bound — skipped");
            return;
        }
    };
    let key = record.r2_key();
    let json = match serde_json::to_string(&record) {
        Ok(j) => j,
        Err(err) => {
            worker::console_error!("recording: serialize failed: {err}");
            return;
        }
    };
    // Synchronous breadcrumb — flushes with the response, never swallowed.
    // Confirms we reached the put-scheduling point and with what key/size.
    worker::console_log!(
        "recording: scheduling R2 put key={key} bytes={}",
        json.len()
    );
    ctx.wait_until(async move {
        match bucket.put(key.clone(), json).execute().await {
            Ok(_) => worker::console_log!("recording: R2 put OK key={key}"),
            Err(err) => worker::console_error!("recording: R2 put failed key={key}: {err}"),
        }
    });
}

/// Native stub so the crate builds and tests run off-wasm. Drops the
/// record (no R2 in native tests).
#[cfg(not(target_arch = "wasm32"))]
pub fn record_to_r2(_env: &worker::Env, _ctx: &worker::Context, _record: RequestRecord) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_collects_in_order_and_drains() {
        begin();
        push("log", "first".into());
        push("error", "second".into());
        let logs = take_logs();
        assert_eq!(
            logs,
            vec![
                LogLine {
                    level: "log",
                    msg: "first".into()
                },
                LogLine {
                    level: "error",
                    msg: "second".into()
                },
            ]
        );
        // Drained: a second take is empty.
        assert!(take_logs().is_empty());
    }

    #[test]
    fn begin_clears_prior_request() {
        push("log", "stale".into());
        begin();
        assert!(take_logs().is_empty());
    }

    #[test]
    fn rlog_macro_buffers() {
        begin();
        rlog!("hello {}", "world");
        rlog_err!("boom {}", 42);
        let logs = take_logs();
        assert_eq!(logs.len(), 2);
        assert_eq!(
            logs[0],
            LogLine {
                level: "log",
                msg: "hello world".into()
            }
        );
        assert_eq!(
            logs[1],
            LogLine {
                level: "error",
                msg: "boom 42".into()
            }
        );
    }
}
