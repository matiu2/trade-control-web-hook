//! Minimal `tracing::Subscriber` that forwards events to Cloudflare's
//! `console.log` / `console.error` via the `worker` crate's macros.
//!
//! Why: broker crates (notably `broker-tradenation`) log error detail
//! through `tracing::warn!` / `tracing::error!`. Without a subscriber
//! installed those events are silently dropped in wasm, so the worker's
//! own lossy `entry failed: broker rejected the order` line is the only
//! breadcrumb operators see. This subscriber surfaces the underlying
//! reason in Cloudflare's request log.
//!
//! Scope is deliberately narrow:
//! - Events only (no span tracking) — that's all the broker crates emit.
//! - One short-lived format string per event: `LEVEL target: field=value …`.
//! - `WARN`/`ERROR` route to `console_error!`, everything else to
//!   `console_log!`.

use core::sync::atomic::{AtomicU64, Ordering};
use std::fmt::Write;

use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
    span,
};
use worker::{console_error, console_log};

pub struct ConsoleSubscriber;

impl ConsoleSubscriber {
    /// Install once. Repeat calls are a no-op (the global default is
    /// already set). Safe to call from each request handler.
    pub fn install() {
        use std::sync::OnceLock;
        static INSTALLED: OnceLock<()> = OnceLock::new();
        INSTALLED.get_or_init(|| {
            // Best-effort: if a subscriber was set elsewhere this returns
            // an Err and we accept the loss rather than panicking.
            let _ = tracing::subscriber::set_global_default(ConsoleSubscriber);
        });
    }
}

impl Subscriber for ConsoleSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
        // We don't track spans; hand out monotonic ids so callers that
        // hold an Id (e.g. for enter/exit) don't see duplicates.
        static NEXT: AtomicU64 = AtomicU64::new(1);
        span::Id::from_u64(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}
    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}
    fn enter(&self, _span: &span::Id) {}
    fn exit(&self, _span: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let meta = event.metadata();
        let mut buf = String::new();
        let _ = write!(&mut buf, "{} {}: ", meta.level(), meta.target());
        let mut visitor = FmtVisitor {
            buf: &mut buf,
            first: true,
        };
        event.record(&mut visitor);

        if meta.level() <= &tracing::Level::WARN {
            console_error!("{buf}");
        } else {
            console_log!("{buf}");
        }
    }
}

struct FmtVisitor<'a> {
    buf: &'a mut String,
    first: bool,
}

impl FmtVisitor<'_> {
    fn sep(&mut self) {
        if self.first {
            self.first = false;
        } else {
            self.buf.push(' ');
        }
    }
}

impl Visit for FmtVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.sep();
        if field.name() == "message" {
            let _ = write!(self.buf, "{value:?}");
        } else {
            let _ = write!(self.buf, "{}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.sep();
        if field.name() == "message" {
            self.buf.push_str(value);
        } else {
            let _ = write!(self.buf, "{}={value}", field.name());
        }
    }
}
