//! Background jobs so the slow shell-outs (`replay-candles`, `plan timeline`)
//! don't freeze the render loop. Each job runs the blocking `cli::*` call on its
//! own `std::thread` and posts a [`JobResult`] back over an mpsc channel. The
//! event loop drains the channel every tick (see `main.rs`), and `App` applies
//! the result to its cache.
//!
//! No async runtime: the CLIs are blocking subprocesses, so a plain thread per
//! job is the simplest thing that keeps the UI live. Jobs are short-lived and
//! few (one replay / one timeline-load at a time per plan), so the thread count
//! never grows unbounded.

use std::sync::mpsc::Sender;
use std::thread;

use crate::cli;

/// Which slow fetch a job performs. Used both as the in-flight marker (so the UI
/// can show "loading…" and we don't double-spawn) and to route the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobKind {
    /// `plan export` + `plan timeline` — fills detail + timeline for a plan.
    Timeline,
    /// `replay-candles --plan` — the ~25s replay run.
    Replay,
    /// `replay-candles --plan --annotate` — draw positions on the live chart.
    LoadTv,
}

impl JobKind {
    /// A human label for the "loading…" line.
    pub fn verb(self) -> &'static str {
        match self {
            JobKind::Timeline => "loading timeline",
            JobKind::Replay => "running replay",
            JobKind::LoadTv => "loading TradingView",
        }
    }
}

/// The outcome of a finished background job, sent back to the event loop.
#[derive(Debug)]
pub struct JobResult {
    pub trade_id: String,
    pub kind: JobKind,
    pub outcome: JobOutcome,
}

/// The payload of a finished job — the loaded data or an error message.
#[derive(Debug)]
pub enum JobOutcome {
    /// `plan export` JSON + `plan timeline` JSON (in that order).
    Timeline {
        export_json: String,
        timeline_json: String,
    },
    /// The replay report text.
    Replay(String),
    /// TradingView annotate finished (no payload — the draw is a side effect).
    LoadTv,
    /// The job failed; the string is the error to surface in the footer.
    Failed(String),
}

/// Spawn the timeline-load job: `plan export` then `plan timeline`, both on a
/// worker thread. Sends one [`JobResult`] when done.
pub fn spawn_timeline(tx: Sender<JobResult>, trade_id: String) {
    spawn(tx, trade_id.clone(), JobKind::Timeline, move || {
        let export_json = cli::plan_export_json(&trade_id)?;
        let timeline_json = cli::plan_timeline_json(&trade_id)?;
        Ok(JobOutcome::Timeline {
            export_json,
            timeline_json,
        })
    });
}

/// Spawn the replay job. `export_json` is the already-fetched plan body (written
/// to a temp file here so the worker thread does no shared-state reads).
/// `source` is the plan's broker as a `replay-candles --source` value
/// (`oanda`/`tradenation`) — it must match the plan's broker or instrument
/// resolution fails (an OANDA-only ratio like XAU/XAG isn't on TradeNation).
pub fn spawn_replay(
    tx: Sender<JobResult>,
    trade_id: String,
    export_json: String,
    source: Option<String>,
) {
    spawn(tx, trade_id.clone(), JobKind::Replay, move || {
        let path = write_plan(&trade_id, "replay", &export_json)?;
        let report = cli::replay(&path, false, source.as_deref())?;
        Ok(JobOutcome::Replay(report))
    });
}

/// Spawn the TradingView **load** job — set the live chart's symbol + timeframe
/// for this plan. The operator scrolls/zooms to the setup manually; no
/// scroll-to-anchor, no range, no drawing. `instrument`/`granularity` come from
/// the plan row; `broker` from the fetched detail (drives the exchange prefix).
pub fn spawn_load_tv(
    tx: Sender<JobResult>,
    trade_id: String,
    instrument: String,
    broker: String,
    granularity: String,
) {
    spawn(tx, trade_id, JobKind::LoadTv, move || {
        crate::tv::load_chart(&instrument, &broker, &granularity)?;
        Ok(JobOutcome::LoadTv)
    });
}

/// Write a plan body to a per-purpose temp file for `replay-candles --plan`.
fn write_plan(
    trade_id: &str,
    purpose: &str,
    export_json: &str,
) -> color_eyre::Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("journal-{purpose}-{trade_id}.json"));
    std::fs::write(&path, export_json)?;
    Ok(path)
}

/// Run `work` on a new thread, mapping its `Result` into a `JobResult` and
/// sending it. A send error means the receiver (the app) is gone — we're
/// shutting down, so drop the result silently.
fn spawn<F>(tx: Sender<JobResult>, trade_id: String, kind: JobKind, work: F)
where
    F: FnOnce() -> color_eyre::Result<JobOutcome> + Send + 'static,
{
    thread::spawn(move || {
        let outcome = work().unwrap_or_else(|e| JobOutcome::Failed(e.to_string()));
        tx.send(JobResult {
            trade_id,
            kind,
            outcome,
        })
        .ok();
    });
}
