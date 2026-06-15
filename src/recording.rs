//! Per-request event recording to R2.
//!
//! Every inbound request is captured as a single JSON object: the
//! verbatim signed body, the request headers, the final HTTP response
//! (status + outcome), and **every** log line the handler emitted during
//! dispatch. The object is written to R2 asynchronously via
//! `ctx.wait_until` so recording adds no latency to the response.
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

use serde::Serialize;

/// One captured log line.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LogLine {
    /// `"log"` or `"error"` — mirrors console.log vs console.error.
    pub level: &'static str,
    /// The formatted message.
    pub msg: String,
}

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

/// The full per-request record written to R2 as one JSON object.
#[derive(Debug, Serialize)]
pub struct RequestRecord {
    /// UTC RFC3339 — when the request was received.
    pub ts: String,
    /// The request id minted for this invocation (correlates logs).
    pub request_id: String,
    /// HTTP method + path.
    pub method: String,
    pub path: String,
    /// Request headers (verbatim).
    pub headers: Vec<(String, String)>,
    /// The verbatim request body (signed YAML for intents).
    pub body: String,
    /// The intent id, if the body parsed to a verified intent.
    pub intent_id: Option<String>,
    /// The trade id, if present on the intent.
    pub trade_id: Option<String>,
    /// Final HTTP status returned to the caller.
    pub status: u16,
    /// Short outcome string (e.g. `"entered"`, `"rejected: missing-prep"`).
    pub outcome: String,
    /// Every log line emitted during dispatch, in order.
    pub logs: Vec<LogLine>,
}

impl RequestRecord {
    /// R2 object key. Layout: `req/<date>/<ts>-<request_id>.json` so a
    /// day's requests list under one prefix and sort by time. Trade-keyed
    /// reconstruction filters on the `trade_id` field, not the key.
    ///
    /// Only called from the wasm `record_to_r2`; dead on native.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn r2_key(&self) -> String {
        // ts is RFC3339 like 2026-06-15T07:51:39.123Z; take the date.
        let date = self.ts.get(..10).unwrap_or("unknown");
        format!(
            "req/{date}/{ts}-{rid}.json",
            ts = self.ts,
            rid = self.request_id
        )
    }
}

/// R2 bucket binding name. Add `[[r2_buckets]] binding = "TRADE_CONTROL_R2"`
/// to `wrangler.toml` and create the bucket before deploying. If the
/// binding is absent, recording is skipped (fail-soft) — the request
/// still succeeds. Only read by the wasm `record_to_r2`; dead on native.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const R2_BINDING: &str = "TRADE_CONTROL_R2";

/// Mint a short request id. We have no RNG in the worker
/// (`Math::random`/`Date::now` are restricted in some contexts), so derive
/// a stable-but-unique id from a cheap hash of the body + headers. Two
/// distinct requests almost never collide; identical replays of the same
/// body intentionally hash the same, which is fine (the ts disambiguates
/// the R2 key).
pub fn mint_request_id(body: &str, headers: &[(String, String)]) -> String {
    // FNV-1a over body then header bytes. Not cryptographic — just a
    // compact correlation handle for logs and the R2 key.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    mix(body.as_bytes());
    for (k, v) in headers {
        mix(k.as_bytes());
        mix(v.as_bytes());
    }
    format!("{h:016x}")
}

/// Best-effort extract `id:` and `trade_id:` from the signed YAML body so
/// the R2 record is filterable without re-parsing the whole intent. Line
/// scan, not a YAML parse — the body is recorded verbatim anyway, so this
/// is only an indexing convenience and a malformed body just yields None.
pub fn ids_from_body(body: &str) -> (Option<String>, Option<String>) {
    let mut id = None;
    let mut trade_id = None;
    for line in body.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("id:") {
            if id.is_none() {
                id = Some(unquote(v));
            }
        } else if let Some(v) = t.strip_prefix("trade_id:") {
            trade_id = Some(unquote(v));
        }
    }
    (id, trade_id)
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches(['"', '\'']).to_string()
}

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

    #[test]
    fn ids_parsed_from_body() {
        let body =
            "action: enter\nid: ihs-eur-usd-abc123\ntrade_id: ihs-eur-usd\ninstrument: EUR_USD\n";
        let (id, trade_id) = ids_from_body(body);
        assert_eq!(id.as_deref(), Some("ihs-eur-usd-abc123"));
        assert_eq!(trade_id.as_deref(), Some("ihs-eur-usd"));
    }

    #[test]
    fn ids_handle_quotes_and_absence() {
        let (id, trade_id) = ids_from_body("id: \"q-1\"\n");
        assert_eq!(id.as_deref(), Some("q-1"));
        assert_eq!(trade_id, None);
    }

    #[test]
    fn request_id_is_stable_and_varies() {
        let h = vec![("x".to_string(), "y".to_string())];
        let a = mint_request_id("body-a", &h);
        let b = mint_request_id("body-b", &h);
        assert_ne!(a, b, "different bodies → different ids");
        assert_eq!(a, mint_request_id("body-a", &h), "same input → same id");
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn r2_key_is_date_partitioned() {
        let rec = RequestRecord {
            ts: "2026-06-15T07:51:39.123Z".into(),
            request_id: "abc123".into(),
            method: "POST".into(),
            path: "/".into(),
            headers: vec![],
            body: String::new(),
            intent_id: None,
            trade_id: None,
            status: 200,
            outcome: "ok".into(),
            logs: vec![],
        };
        assert_eq!(
            rec.r2_key(),
            "req/2026-06-15/2026-06-15T07:51:39.123Z-abc123.json"
        );
    }
}
