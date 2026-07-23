//! App state + the transitions the event loop drives. Keeps all business logic
//! (what to fetch on a screen push, the delete guard) here so `main.rs` is a
//! thin render/input loop.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};

use color_eyre::eyre::Result;

use crate::cli;
use crate::jobs::{self, JobKind, JobOutcome, JobResult};
use crate::plan::{PlanDetail, PlanRow, parse_plan_export, parse_plan_list};
use crate::screen::Screen;

/// A transient status/error message shown in the footer.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub text: String,
    pub is_error: bool,
}

impl Status {
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }
    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// Everything loaded for one opened plan, filled lazily as screens are pushed.
#[derive(Debug, Clone, Default)]
pub struct PlanData {
    /// Info-bar facts (from `plan export`), fetched on the Timeline push.
    pub detail: Option<PlanDetail>,
    /// Raw timeline JSON (from `plan timeline`), fetched on the Timeline push.
    pub timeline_json: Option<String>,
    /// Raw `plan export` JSON — the detail popup's full dump.
    pub export_json: Option<String>,
    /// The replay report (from `replay-candles`), filled on the Replay push.
    pub replay_report: Option<String>,
    /// Deepest screen ever reached for this plan (delete guard reads this).
    pub max_depth: u8,
}

/// A confirmation the operator must answer before a destructive action.
#[derive(Debug, Clone)]
pub struct Confirm {
    pub trade_id: String,
    pub prompt: String,
}

pub struct App {
    pub plans: Vec<PlanRow>,
    pub selected: usize,
    pub screen: Screen,
    /// Per-plan loaded data, keyed by trade_id.
    pub data: HashMap<String, PlanData>,
    pub status: Status,
    pub show_popup: bool,
    pub confirm: Option<Confirm>,
    pub should_quit: bool,
    /// Sender handed to background job threads; results arrive on `job_rx`.
    job_tx: Sender<JobResult>,
    /// Receiver drained each tick by [`App::drain_jobs`].
    job_rx: Receiver<JobResult>,
    /// Jobs currently running, so we show "loading…" and never double-spawn.
    in_flight: HashSet<(String, JobKind)>,
    /// Monotonic tick, bumped each event-loop pass, to animate the spinner.
    pub tick: u64,
    /// Vertical scroll offset (in lines) of the `i` detail popup.
    pub popup_scroll: u16,
    /// The journal DB connection, opened lazily on the first `s` record and
    /// cached for the session (see [`App::record_db`]).
    record_db: Option<rusqlite::Connection>,
}

/// Braille spinner frames for the "loading…" indicator.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

impl App {
    /// Build the app, fetching the initial plan list.
    pub fn new() -> Result<Self> {
        let plans = fetch_plans()?;
        let (job_tx, job_rx) = channel();
        Ok(Self {
            plans,
            selected: 0,
            screen: Screen::List,
            data: HashMap::new(),
            status: Status::info("loaded plans"),
            show_popup: false,
            confirm: None,
            should_quit: false,
            job_tx,
            job_rx,
            in_flight: HashSet::new(),
            tick: 0,
            popup_scroll: 0,
            record_db: None,
        })
    }

    /// True while any background job for the current plan is running — the UI
    /// reads this to show a spinner / "loading…" line.
    pub fn is_current_loading(&self, kind: JobKind) -> bool {
        self.current_plan()
            .map(|p| self.in_flight.contains(&(p.trade_id.clone(), kind)))
            .unwrap_or(false)
    }

    /// True if any job at all is in flight for the current plan.
    pub fn current_busy(&self) -> Option<JobKind> {
        let trade_id = self.current_plan()?.trade_id.clone();
        [JobKind::Timeline, JobKind::Replay, JobKind::LoadTv]
            .into_iter()
            .find(|k| self.in_flight.contains(&(trade_id.clone(), *k)))
    }

    /// The current spinner glyph (advances with `tick`).
    pub fn spinner(&self) -> char {
        SPINNER[(self.tick as usize) % SPINNER.len()]
    }

    /// The currently-highlighted plan (list) or the open plan (deeper screens).
    pub fn current_plan(&self) -> Option<&PlanRow> {
        self.plans.get(self.selected)
    }

    /// Loaded data for the current plan, if any.
    pub fn current_data(&self) -> Option<&PlanData> {
        self.current_plan().and_then(|p| self.data.get(&p.trade_id))
    }

    // -- list navigation ---------------------------------------------------

    pub fn select_next(&mut self) {
        if !self.plans.is_empty() {
            self.selected = (self.selected + 1) % self.plans.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.plans.is_empty() {
            self.selected = (self.selected + self.plans.len() - 1) % self.plans.len();
        }
    }

    // -- screen stack ------------------------------------------------------

    /// Push one screen deeper, kicking off that screen's fetch (as a background
    /// job) the first time it's reached for this plan. Returns immediately — the
    /// job posts its result to `drain_jobs` when done.
    pub fn push_deeper(&mut self) {
        let Some(next) = self.screen.deeper() else {
            return;
        };
        // A plan must be selected to leave the list.
        if self.current_plan().is_none() {
            return;
        }
        self.screen = next;
        self.record_depth(next.depth());
        self.start_screen_jobs(next);
    }

    /// Pop one screen shallower. From the list this is a no-op.
    pub fn pop_shallower(&mut self) {
        if let Some(prev) = self.screen.shallower() {
            self.screen = prev;
        }
    }

    /// Record that the current plan reached at least `depth`.
    fn record_depth(&mut self, depth: u8) {
        if let Some(p) = self.plans.get(self.selected) {
            let entry = self.data.entry(p.trade_id.clone()).or_default();
            entry.max_depth = entry.max_depth.max(depth);
        }
    }

    /// Kick off (as background jobs) whatever a freshly-entered screen needs,
    /// skipping anything already cached or already in flight.
    fn start_screen_jobs(&mut self, screen: Screen) {
        let Some(trade_id) = self.current_plan().map(|p| p.trade_id.clone()) else {
            return;
        };
        match screen {
            Screen::Timeline => {
                self.start_timeline(&trade_id);
                // Auto-load TradingView on reaching the first deep screen. If the
                // detail is already cached this fires immediately; otherwise the
                // timeline job's completion (apply_job) fires it once loaded.
                self.start_load_tv(&trade_id);
            }
            Screen::Replay => self.start_replay(&trade_id),
            Screen::Compare => {
                // Compare needs both; each is a no-op if already cached/running.
                self.start_timeline(&trade_id);
                self.start_replay(&trade_id);
            }
            Screen::List => {}
        }
    }

    /// Spawn the timeline-load job (export + timeline) unless cached or running.
    fn start_timeline(&mut self, trade_id: &str) {
        let cached = self
            .data
            .get(trade_id)
            .map(|d| d.timeline_json.is_some() && d.export_json.is_some())
            .unwrap_or(false);
        if cached || !self.mark_in_flight(trade_id, JobKind::Timeline) {
            return;
        }
        self.status = Status::info(format!("{trade_id}: loading timeline…"));
        jobs::spawn_timeline(self.job_tx.clone(), trade_id.to_string());
    }

    /// Spawn the replay job unless cached or running. Needs the plan export; if
    /// it isn't cached yet, the timeline job will fetch it — so we require it
    /// here and let a not-yet-loaded plan spawn the timeline first.
    fn start_replay(&mut self, trade_id: &str) {
        let cached = self
            .data
            .get(trade_id)
            .map(|d| d.replay_report.is_some())
            .unwrap_or(false);
        if cached {
            return;
        }
        let Some(export) = self.data.get(trade_id).and_then(|d| d.export_json.clone()) else {
            // No export yet — ensure the timeline job runs to fetch it; the
            // replay is retried when we re-enter/refresh once it's cached.
            self.start_timeline(trade_id);
            return;
        };
        if !self.mark_in_flight(trade_id, JobKind::Replay) {
            return;
        }
        self.status = Status::info(format!("{trade_id}: running replay…"));
        jobs::spawn_replay(self.job_tx.clone(), trade_id.to_string(), export);
    }

    /// Add a job to the in-flight set. Returns `false` if it was already there
    /// (so the caller skips a duplicate spawn).
    fn mark_in_flight(&mut self, trade_id: &str, kind: JobKind) -> bool {
        self.in_flight.insert((trade_id.to_string(), kind))
    }

    /// Drain any finished background jobs and apply their results to the cache.
    /// Called once per event-loop tick (see `main.rs`). Returns true if any job
    /// completed (so the loop knows a redraw is worthwhile).
    pub fn drain_jobs(&mut self) -> bool {
        let mut any = false;
        while let Ok(result) = self.job_rx.try_recv() {
            any = true;
            self.in_flight
                .remove(&(result.trade_id.clone(), result.kind));
            self.apply_job(result);
        }
        any
    }

    /// Apply one finished job's outcome to the plan's cached data + status.
    fn apply_job(&mut self, result: JobResult) {
        let JobResult {
            trade_id,
            kind,
            outcome,
        } = result;
        match outcome {
            JobOutcome::Timeline {
                export_json,
                timeline_json,
            } => {
                let detail = parse_plan_export(&export_json).ok();
                let entry = self.data.entry(trade_id.clone()).or_default();
                entry.export_json = Some(export_json);
                entry.detail = detail;
                entry.timeline_json = Some(timeline_json);
                self.status = Status::info(format!("{trade_id}: timeline loaded"));
                // These only matter while this plan is the open one on a deep
                // screen — not for a background prefetch.
                let is_open = self
                    .current_plan()
                    .map(|p| p.trade_id == trade_id)
                    .unwrap_or(false);
                if is_open {
                    // Auto-load TradingView the first time we reach a deep screen
                    // (the detail with the anchor time only exists now).
                    if self.screen.depth() >= Screen::Timeline.depth() {
                        self.start_load_tv(&trade_id);
                    }
                    // A replay may have been requested before the export existed;
                    // if we're on/at Replay or Compare, kick it now.
                    if matches!(self.screen, Screen::Replay | Screen::Compare) {
                        self.start_replay(&trade_id);
                    }
                }
            }
            JobOutcome::Replay(report) => {
                self.data.entry(trade_id.clone()).or_default().replay_report = Some(report);
                self.status = Status::info(format!("{trade_id}: replay done"));
            }
            JobOutcome::LoadTv => {
                self.status = Status::info(format!("{trade_id}: loaded in TradingView"));
            }
            JobOutcome::Failed(msg) => {
                self.status = Status::error(format!("{trade_id} {}: {msg}", kind.verb()));
            }
        }
    }

    // -- actions -----------------------------------------------------------

    /// Load the current plan into TradingView (the `l` key) — navigate the live
    /// chart to this setup (symbol + timeframe + scroll-to-anchor + zoom-out),
    /// as a background job so the ~few-second navigation doesn't freeze the UI.
    /// Also auto-fired once when the Timeline screen is first reached.
    pub fn load_tv(&mut self) {
        let Some(trade_id) = self.current_plan().map(|p| p.trade_id.clone()) else {
            return;
        };
        self.start_load_tv(&trade_id);
    }

    /// Spawn the TradingView-load job for `trade_id` if the plan detail (which
    /// carries the anchor time) is loaded. If it isn't yet, kick the timeline
    /// job; `apply_job` re-tries the load when the detail lands.
    fn start_load_tv(&mut self, trade_id: &str) {
        // Instrument + granularity come from the list row; anchor from detail.
        let Some(row) = self.plans.iter().find(|p| p.trade_id == trade_id) else {
            return;
        };
        let instrument = row.instrument.clone();
        let granularity = row.granularity.clone();
        // Anchor + broker both come from the fetched detail.
        let detail = self.data.get(trade_id).and_then(|d| d.detail.as_ref());
        let anchor = detail.and_then(|d| d.armed_at.clone());
        let broker = detail.map(|d| d.broker.clone()).unwrap_or_default();
        let Some(anchor) = anchor else {
            // Detail (with armed_at + broker) not loaded yet — fetch it; the
            // Timeline completion will fire the load.
            self.start_timeline(trade_id);
            return;
        };
        if !self.mark_in_flight(trade_id, JobKind::LoadTv) {
            return;
        }
        self.status = Status::info(format!("{trade_id}: loading TradingView…"));
        jobs::spawn_load_tv(
            self.job_tx.clone(),
            trade_id.to_string(),
            instrument,
            broker,
            granularity,
            anchor,
        );
    }

    /// Record the current plan's outcome to the journal DB (the `s` key). Only
    /// meaningful once both outcomes are loaded, so it requires the replay
    /// report (which implies the timeline/detail were fetched first). The
    /// real-life and replay outcomes are stored in separate columns.
    pub fn record_current(&mut self) {
        let Some(trade_id) = self.current_plan().map(|p| p.trade_id.clone()) else {
            return;
        };
        let Some(data) = self.data.get(&trade_id) else {
            self.status = Status::error("open the plan first (→) before recording");
            return;
        };
        let (Some(detail), Some(timeline), Some(replay)) = (
            data.detail.as_ref(),
            data.timeline_json.as_deref(),
            data.replay_report.as_deref(),
        ) else {
            self.status =
                Status::error("run the replay (→ to Replay) before recording — need both outcomes");
            return;
        };
        let now = chrono::Utc::now().to_rfc3339();
        let rec = crate::record::TradeRecord::from_plan(detail, timeline, replay, now);
        match self.record_db() {
            Ok(conn) => match crate::record::upsert(conn, &rec) {
                Ok(_) => {
                    self.status = Status::info(format!(
                        "recorded {trade_id} — live: {} / replay net R: {}",
                        rec.live_outcome,
                        rec.replay_net_r.as_deref().unwrap_or("n/a"),
                    ))
                }
                Err(e) => self.status = Status::error(format!("record: {e}")),
            },
            Err(e) => self.status = Status::error(format!("open journal db: {e}")),
        }
    }

    /// The journal DB connection, opened + migrated lazily on first record and
    /// cached for the session.
    fn record_db(&mut self) -> Result<&rusqlite::Connection> {
        if self.record_db.is_none() {
            let path = crate::record::db_path();
            self.record_db = Some(crate::record::open_db(&path)?);
        }
        // Safe: just ensured Some above.
        self.record_db
            .as_ref()
            .ok_or_else(|| color_eyre::eyre::eyre!("journal db unavailable"))
    }

    /// Request a replay re-run (the `r` key), bypassing the cache.
    pub fn rerun_replay(&mut self) {
        let Some(trade_id) = self.current_plan().map(|p| p.trade_id.clone()) else {
            return;
        };
        if let Some(d) = self.data.get_mut(&trade_id) {
            d.replay_report = None;
        }
        self.start_replay(&trade_id);
    }

    /// Ask to delete the current plan. Guarded: only allowed once the plan has
    /// been drilled into at least one screen (max_depth ≥ 1).
    pub fn request_delete(&mut self) {
        let Some(plan) = self.current_plan() else {
            return;
        };
        let trade_id = plan.trade_id.clone();
        let depth = self.data.get(&trade_id).map(|d| d.max_depth).unwrap_or(0);
        if depth < 1 {
            self.status = Status::error("open the plan (→) before deleting");
            return;
        }
        self.confirm = Some(Confirm {
            prompt: format!("Delete plan {trade_id}? (y/n)"),
            trade_id,
        });
    }

    /// Answer the pending confirm. `yes` performs the delete + refresh.
    pub fn resolve_confirm(&mut self, yes: bool) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        if !yes {
            self.status = Status::info("delete cancelled");
            return;
        }
        match cli::plan_delete(&confirm.trade_id) {
            Ok(_) => {
                self.data.remove(&confirm.trade_id);
                self.screen = Screen::List;
                match fetch_plans() {
                    Ok(plans) => {
                        self.plans = plans;
                        if self.selected >= self.plans.len() {
                            self.selected = self.plans.len().saturating_sub(1);
                        }
                        self.status = Status::info(format!("deleted {}", confirm.trade_id));
                    }
                    Err(e) => self.status = Status::error(format!("refresh after delete: {e}")),
                }
            }
            Err(e) => self.status = Status::error(format!("delete: {e}")),
        }
    }

    pub fn toggle_popup(&mut self) {
        self.show_popup = !self.show_popup;
        // Always start a freshly-opened popup at the top.
        self.popup_scroll = 0;
    }

    /// Scroll the detail popup by `delta` lines (negative = up), clamped at 0.
    /// The bottom is bounded by the render (it won't scroll past the content).
    pub fn scroll_popup(&mut self, delta: i32) {
        let next = self.popup_scroll as i32 + delta;
        self.popup_scroll = next.max(0) as u16;
    }

    /// Jump the popup to the top.
    pub fn scroll_popup_home(&mut self) {
        self.popup_scroll = 0;
    }

    /// Jump the popup near the bottom. The exact clamp happens at render time
    /// (it knows the content height); `u16::MAX` here just means "as far down as
    /// it goes", and the renderer pins it to the last page.
    pub fn scroll_popup_end(&mut self) {
        self.popup_scroll = u16::MAX;
    }
}

/// Fetch + parse the plan list.
fn fetch_plans() -> Result<Vec<PlanRow>> {
    let yaml = cli::plan_list_yaml()?;
    parse_plan_list(&yaml)
}

#[cfg(test)]
impl App {
    /// Build an app from already-parsed rows, without touching the network —
    /// for render tests against fixtures.
    pub fn from_rows(plans: Vec<PlanRow>) -> Self {
        let (job_tx, job_rx) = channel();
        Self {
            plans,
            selected: 0,
            screen: Screen::List,
            data: HashMap::new(),
            status: Status::info("test"),
            show_popup: false,
            confirm: None,
            should_quit: false,
            job_tx,
            job_rx,
            in_flight: HashSet::new(),
            tick: 0,
            popup_scroll: 0,
            record_db: None,
        }
    }

    /// Seed the current plan's cached data (detail + timeline) so deeper-screen
    /// render tests have something to draw.
    pub fn seed_current(&mut self, data: PlanData) {
        if let Some(p) = self.plans.get(self.selected) {
            self.data.insert(p.trade_id.clone(), data);
        }
    }

    /// Force the visible screen (test helper).
    pub fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
    }

    /// Move the selection to the plan with the given trade_id (test helper).
    pub fn select_to(&mut self, trade_id: &str) {
        if let Some(i) = self.plans.iter().position(|p| p.trade_id == trade_id) {
            self.selected = i;
        }
    }

    /// Post a job result as if a background thread finished it (test helper).
    pub fn inject_job(&mut self, result: JobResult) {
        self.job_tx.send(result).ok();
    }

    /// Mark a job in-flight without spawning a thread (test helper).
    pub fn mark_in_flight_test(&mut self, trade_id: &str, kind: JobKind) {
        self.in_flight.insert((trade_id.to_string(), kind));
    }

    /// Read the in-flight set size (test helper).
    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    /// Point the journal DB at a fresh in-memory database (test helper) so
    /// `record_current` writes there instead of `~/.config`.
    pub fn use_in_memory_db(&mut self) {
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory db");
        crate::record::migrate_for_test(&conn);
        self.record_db = Some(conn);
    }

    /// Count rows in the journal DB (test helper). Panics if no DB is set.
    pub fn recorded_count(&self) -> i64 {
        self.record_db
            .as_ref()
            .expect("db set")
            .query_row("SELECT COUNT(*) FROM trades", [], |r| r.get(0))
            .expect("count")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobOutcome;
    use crate::plan::PlanRow;

    fn row(trade_id: &str) -> PlanRow {
        PlanRow {
            trade_id: trade_id.to_string(),
            account: "acct".into(),
            instrument: "AUD_CAD".into(),
            granularity: "h1".into(),
            phase: Some("await_entry".into()),
            shadow: false,
            archived_at: None,
            watermark: None,
        }
    }

    #[test]
    fn drain_applies_timeline_and_clears_in_flight() {
        let mut app = App::from_rows(vec![row("t1")]);
        app.mark_in_flight_test("t1", JobKind::Timeline);
        assert_eq!(app.in_flight_len(), 1);

        app.inject_job(JobResult {
            trade_id: "t1".into(),
            kind: JobKind::Timeline,
            outcome: JobOutcome::Timeline {
                export_json: r#"{"trade_id":"t1","instrument":"AUD_CAD","direction":"short","granularity":"h1","rules":[{"rule_id":"05-enter","intent":{"entry":{"type":"stop"}}}]}"#.into(),
                timeline_json: r#"{"records":[],"ticks":[]}"#.into(),
            },
        });

        let changed = app.drain_jobs();
        assert!(changed, "drain reports a completed job");
        // In-flight cleared, data cached, entry-mode classified.
        assert_eq!(app.in_flight_len(), 0);
        let data = app.data.get("t1").expect("cached");
        assert!(data.timeline_json.is_some());
        assert!(data.export_json.is_some());
        assert!(data.detail.is_some(), "export parsed into detail");
    }

    #[test]
    fn drain_surfaces_failure_in_status() {
        let mut app = App::from_rows(vec![row("t1")]);
        app.mark_in_flight_test("t1", JobKind::Replay);
        app.inject_job(JobResult {
            trade_id: "t1".into(),
            kind: JobKind::Replay,
            outcome: JobOutcome::Failed("boom".into()),
        });
        app.drain_jobs();
        assert!(app.status.is_error);
        assert!(app.status.text.contains("boom"));
        assert_eq!(app.in_flight_len(), 0, "failed job also clears in-flight");
    }

    #[test]
    fn drain_noop_when_empty() {
        let mut app = App::from_rows(vec![row("t1")]);
        assert!(!app.drain_jobs());
    }

    const EXPORT: &str = include_str!("../tests/fixtures/plan_export.json");
    const TIMELINE: &str = include_str!("../tests/fixtures/plan_timeline.json");
    const REPLAY: &str = include_str!("../tests/fixtures/replay_report.txt");

    /// Recording before the replay ran is rejected (need both outcomes), and no
    /// row is written.
    #[test]
    fn record_requires_replay_report() {
        let mut app = App::from_rows(vec![row("hs-aud-cad-a07622da")]);
        app.use_in_memory_db();
        app.seed_current(PlanData {
            detail: parse_plan_export(EXPORT).ok(),
            export_json: Some(EXPORT.to_string()),
            timeline_json: Some(TIMELINE.to_string()),
            replay_report: None, // replay not run yet
            max_depth: 1,
        });
        app.record_current();
        assert!(app.status.is_error, "no-replay record should error");
        assert_eq!(app.recorded_count(), 0, "nothing written");
    }

    /// With both outcomes loaded, `s` writes exactly one row.
    #[test]
    fn record_writes_a_row_when_both_outcomes_present() {
        let mut app = App::from_rows(vec![row("hs-aud-cad-a07622da")]);
        app.use_in_memory_db();
        app.seed_current(PlanData {
            detail: parse_plan_export(EXPORT).ok(),
            export_json: Some(EXPORT.to_string()),
            timeline_json: Some(TIMELINE.to_string()),
            replay_report: Some(REPLAY.to_string()),
            max_depth: 3,
        });
        app.record_current();
        assert!(!app.status.is_error, "record ok: {}", app.status.text);
        assert!(app.status.text.contains("recorded"), "{}", app.status.text);
        assert_eq!(app.recorded_count(), 1);

        // Recording again upserts — still one row.
        app.record_current();
        assert_eq!(app.recorded_count(), 1, "re-record upserts, no duplicate");
    }
}
