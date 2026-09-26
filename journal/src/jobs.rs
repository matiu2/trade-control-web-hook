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
    /// `tv-arm --save-fixture … replay` — capture the six-cell fixture corpus.
    SaveFixture,
    /// `replay-candles --test-mode --fixture <cell> --rebless` over this plan's
    /// matched cells — recompute their goldens from the frozen inputs.
    Rebless,
    /// `replay-candles --plan <FILE>` — replay the STORED plan as-is, with no
    /// chart read and no re-arm.
    RawReplay,
}

impl JobKind {
    /// A human label for the "loading…" line.
    pub fn verb(self) -> &'static str {
        match self {
            JobKind::Timeline => "loading timeline",
            JobKind::Replay => "running replay",
            JobKind::LoadTv => "loading TradingView",
            JobKind::SaveFixture => "saving fixtures",
            JobKind::Rebless => "re-blessing fixtures",
            JobKind::RawReplay => "running raw replay",
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
    /// TradingView chart is on this plan. `already_there` is true when the chart
    /// was **already** on the right symbol+timeframe and nothing was changed —
    /// worth telling the operator, since it means their scroll position survived.
    LoadTv { already_there: bool },
    /// The fixture-capture report text (tv-arm's per-cell summary).
    SaveFixture(String),
    /// A re-bless run: the per-cell report, and how many of the attempted cells
    /// succeeded. The count is carried separately rather than scraped back out
    /// of the text so the status line can state it without parsing a report.
    Rebless {
        report: String,
        ok: usize,
        total: usize,
    },
    /// The raw (stored-plan) replay's report text.
    RawReplay(String),
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

/// Spawn the replay job. Re-arms the setup from the chart the load job just
/// loaded, via `tv-arm [--spec-url …] --start <armed_at> replay`, so the
/// instrument + broker come from that chart — no plan file, no `--source`, and
/// no resolution failure for OANDA-only assets. `armed_at` is the plan's
/// RFC3339 UTC arm time, used as the `--start` cursor. `skip_flags` are the
/// tv-arm prep-skip flags that reproduce the ORIGINAL plan's prep set (so a
/// skip-BCR plan doesn't re-arm with the full break-and-close-then-retest).
///
/// `spec_url` comes from [`crate::tv::ChartBackend::spec_url`] and must name
/// the SAME backend the load job used — it is what keeps `l` and `r` pointed at
/// one chart. See that method for the divergence it closes.
pub fn spawn_replay(
    tx: Sender<JobResult>,
    trade_id: String,
    armed_at: String,
    skip_flags: Vec<String>,
    spec_url: Option<String>,
) {
    spawn(tx, trade_id, JobKind::Replay, move || {
        let flags: Vec<&str> = skip_flags.iter().map(String::as_str).collect();
        let report = cli::replay_via_tv_arm(&armed_at, &flags, spec_url.as_deref())?;
        Ok(JobOutcome::Replay(report))
    });
}

/// Spawn the fixture-capture job — `tv-arm --save-fixture … replay`, which
/// re-arms from the chart just loaded and writes the six-cell corpus. Same
/// chart precondition and same `skip_flags` caveat as the replay: the capture
/// must reproduce the ORIGINAL plan's prep set or it pins the wrong gates.
/// `fixture_name` is the plan's `trade_id`, so a fixture traces back to its
/// journal page. `message` is the operator's note on why the fixture exists,
/// recorded in each cell's `meta.json`; it is the one field a later
/// `--rebless` will not overwrite, so it is written here or never.
///
/// `spec_url` is the replay's, with higher stakes: a capture off the wrong
/// backend writes a wrong expectation into the committed corpus.
pub fn spawn_save_fixture(
    tx: Sender<JobResult>,
    trade_id: String,
    armed_at: String,
    skip_flags: Vec<String>,
    fixture_name: String,
    message: Option<String>,
    spec_url: Option<String>,
) {
    spawn(tx, trade_id, JobKind::SaveFixture, move || {
        let flags: Vec<&str> = skip_flags.iter().map(String::as_str).collect();
        let report = cli::save_fixture_via_tv_arm(
            &armed_at,
            &flags,
            &fixture_name,
            message.as_deref(),
            spec_url.as_deref(),
        )?;
        Ok(JobOutcome::SaveFixture(report))
    });
}

/// Spawn the re-bless job — `replay-candles --test-mode --fixture <cell>
/// --fixtures-dir <dir> --rebless`, once per cell, on a worker thread.
///
/// `cells` are the directory names [`crate::fixtures::Status::names`] matched
/// for this plan, and **only** those: a re-bless launched from one plan's page
/// must never rewrite another setup's goldens, which is why this takes an
/// explicit list rather than a glob.
///
/// `fixtures_dir` is passed through to every cell's invocation. Resolving it
/// once in the caller and handing it down is deliberate — the CLI's own default
/// walks up from the cwd, and a deployed binary has resolved that to a
/// different checkout before, re-blessing 19 of 63 cells with no error either
/// way.
///
/// **A failing cell does not abort the run.** Each cell's outcome is appended
/// to the report and the loop continues, so one bad cell can't hide the rest —
/// the same contract `replay-candles`' own batch mode keeps.
pub fn spawn_rebless(
    tx: Sender<JobResult>,
    trade_id: String,
    cells: Vec<String>,
    fixtures_dir: std::path::PathBuf,
) {
    spawn(tx, trade_id, JobKind::Rebless, move || {
        let total = cells.len();
        let mut report = String::new();
        let mut ok = 0usize;
        for cell in &cells {
            match cli::rebless_fixture_cell(cell, &fixtures_dir) {
                Ok(out) => {
                    ok += 1;
                    report.push_str(&format!("=== {cell}: re-blessed ===\n{out}\n"));
                }
                Err(e) => report.push_str(&format!("=== {cell}: FAILED ===\n{e}\n")),
            }
        }
        Ok(JobOutcome::Rebless { report, ok, total })
    });
}

/// Spawn the raw-replay job — export the stored plan and feed it straight to
/// `replay-candles --plan`. No chart, no re-arm, so unlike [`spawn_replay`]
/// this has no chart precondition and no skip flags to reproduce: the plan is
/// replayed exactly as the worker holds it.
///
/// `armed_at` becomes `--start`. It is the same cursor the re-armed replay
/// uses, so the two runs cover the same window and stay comparable — and
/// without it `replay-candles` takes the window start from the TradingView
/// chart, which for an expired plan produces a window that runs backwards.
pub fn spawn_raw_replay(
    tx: Sender<JobResult>,
    trade_id: String,
    instrument: String,
    broker: String,
    armed_at: String,
    // The local-chart base URL when that backend is active, so the raw replay
    // paints its positions there instead of shelling out to tv-mcp. `None` on
    // TradingView keeps the `--annotate` path.
    local_chart_url: Option<String>,
) {
    spawn(tx, trade_id.clone(), JobKind::RawReplay, move || {
        let report = cli::raw_replay(
            &trade_id,
            &instrument,
            &broker,
            &armed_at,
            local_chart_url.as_deref(),
        )?;
        Ok(JobOutcome::RawReplay(report))
    });
}

/// Spawn the chart-load job — set the active backend's chart to this plan's
/// symbol + timeframe. The operator scrolls/zooms to the setup manually; no
/// scroll-to-anchor, no range, no drawing. `instrument`/`granularity` come from
/// the plan row; `broker` from the fetched detail (drives the TradingView
/// exchange prefix; unused by the local-chart backend, which is single-broker).
/// `backend` is `App::chart_backend` — TradingView by default, local-chart
/// under `--new-tv` — cloned in at spawn time since the job runs off-thread.
/// `goto` is the plan's `armed_at`, so a local-chart load also CENTRES on the
/// arm bar rather than leaving the operator to hunt for the setup; the
/// TradingView path ignores it (setting a date there was never built).
///
/// Cheap when the chart is already right ON THE TRADINGVIEW PATH: `load_chart`
/// reads the chart first and no-ops if it matches (see `tv`'s module doc). The
/// local-chart backend has no such fast path — see `tv::local_chart`'s module
/// doc for why — so this job always does real work there, still off-thread so
/// the UI doesn't stall on the browser-open.
pub fn spawn_load_tv(
    tx: Sender<JobResult>,
    trade_id: String,
    instrument: String,
    broker: String,
    granularity: String,
    backend: crate::tv::ChartBackend,
    goto: Option<String>,
) {
    spawn(tx, trade_id, JobKind::LoadTv, move || {
        let already_there = crate::tv::load_chart_backend(
            &backend,
            &instrument,
            &broker,
            &granularity,
            goto.as_deref(),
        )?;
        Ok(JobOutcome::LoadTv { already_there })
    });
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
