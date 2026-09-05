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
use crate::search::{self, SearchState};

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
    /// The replay report (from `tv-arm --start … replay`), filled on the Replay
    /// push.
    pub replay_report: Option<String>,
    /// True once the TradingView chart has been loaded to this plan. The replay
    /// re-arms from the live chart, so it must wait for this — otherwise tv-arm
    /// reads whatever chart is up (possibly the prior plan or mid-load).
    pub tv_loaded: bool,
    /// Deepest screen ever reached for this plan (delete guard reads this).
    pub max_depth: u8,
    /// The last fixture-capture report (`s`), if one has run this session.
    pub fixture_report: Option<String>,
}

/// A confirmation the operator must answer before a destructive action.
#[derive(Debug, Clone)]
pub struct Confirm {
    pub trade_id: String,
    pub prompt: String,
}

pub struct App {
    pub plans: Vec<PlanRow>,
    /// Index into the **visible** (search-filtered) rows, not into `plans` —
    /// see [`App::visible`]. Keeping the selection in visible-space means the
    /// highlight, `↑`/`↓`, and `current_plan()` all agree with what's drawn
    /// while a `/` filter is applied.
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
    /// Vertical scroll offset (in lines) of the Replay-screen report body.
    pub replay_scroll: u16,
    /// Set by the refresh key (Ctrl-L): the event loop clears the terminal on
    /// the next frame to repaint from scratch (recovers from any residual screen
    /// corruption, e.g. a stray escape or a resize).
    pub needs_clear: bool,
    /// The `/` search prompt + live query. Filters the list screen.
    pub search: SearchState,
    /// A TV-load was **requested** (`l`, or the replay needing the chart) but
    /// the plan detail — which carries the broker for the exchange prefix —
    /// wasn't loaded yet, so it's parked until the timeline job lands. Nothing
    /// loads the chart unless the operator asked for it: there is no auto-load
    /// on screen entry (removed 2026-07-27).
    tv_load_pending: bool,
    /// A fixture capture (`s`) was **requested** but the chart wasn't loaded (or
    /// the detail wasn't fetched) yet, so it's parked. Separate from
    /// [`Self::tv_load_pending`]: that one means "load the chart", this one means
    /// "capture once the chart is up". Both can be set by a single `s` press.
    save_fixture_pending: bool,
    /// Every fixture cell found under `replay-fixtures/`, scanned once at
    /// startup and re-scanned when a capture completes. Held on the app rather
    /// than read per-frame because the info bar draws many times a second and
    /// the corpus is ~125 directories — a `read_dir` + 125 file reads per frame
    /// would make the TUI visibly stutter.
    pub fixtures: Vec<crate::fixtures::Cell>,
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
            replay_scroll: 0,
            needs_clear: false,
            search: SearchState::default(),
            tv_load_pending: false,
            save_fixture_pending: false,
            fixtures: crate::fixtures::scan(&crate::fixtures::default_dir()),
        })
    }

    /// The fixture-corpus status for the currently-selected plan: which saved
    /// capture (if any) covers this setup. Needs the plan's `armed_at`, which
    /// lives in the detail — so a plan whose timeline hasn't been fetched yet
    /// reports [`fixtures::Status::None`] until it lands.
    pub fn current_fixture_status(&self) -> crate::fixtures::Status {
        let Some(plan) = self.current_plan() else {
            return crate::fixtures::Status::None;
        };
        let armed_at = self
            .data
            .get(&plan.trade_id)
            .and_then(|d| d.detail.as_ref())
            .and_then(|d| d.armed_at.as_deref());
        crate::fixtures::status_for(
            &self.fixtures,
            &plan.instrument,
            &plan.granularity,
            armed_at,
        )
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

    /// Indices into `plans` of the rows the `/` filter lets through, in list
    /// order. No filter → every row. This is the list the UI draws and the one
    /// `selected` indexes, so filtering can never desync the highlight from the
    /// drawn rows.
    pub fn visible(&self) -> Vec<usize> {
        search::matching(&self.plans, &self.search.query)
    }

    /// The visible rows themselves, for the renderer.
    pub fn visible_plans(&self) -> Vec<&PlanRow> {
        self.visible()
            .into_iter()
            .filter_map(|i| self.plans.get(i))
            .collect()
    }

    /// The currently-highlighted plan (list) or the open plan (deeper screens).
    /// `selected` is an index into the **visible** rows.
    pub fn current_plan(&self) -> Option<&PlanRow> {
        let idx = *self.visible().get(self.selected)?;
        self.plans.get(idx)
    }

    /// Loaded data for the current plan, if any.
    pub fn current_data(&self) -> Option<&PlanData> {
        self.current_plan().and_then(|p| self.data.get(&p.trade_id))
    }

    // -- list navigation ---------------------------------------------------

    /// Move down one **visible** row, wrapping.
    pub fn select_next(&mut self) {
        let n = self.visible().len();
        if n > 0 {
            self.select((self.selected + 1) % n);
        }
    }

    /// Move up one **visible** row, wrapping.
    pub fn select_prev(&mut self) {
        let n = self.visible().len();
        if n > 0 {
            self.select((self.selected + n - 1) % n);
        }
    }

    /// Move the selection and fetch whatever the CURRENT screen needs for the
    /// newly-selected plan. Changing rows on a deep screen switches which plan
    /// that screen is showing, so it needs the same fetch a push would do —
    /// without this the renderer sits on "loading timeline…" forever, since
    /// nothing is actually loading (only `push_deeper` used to kick the jobs,
    /// so `←` then `→` was the workaround). No-op on the list, which fetches
    /// nothing.
    fn select(&mut self, index: usize) {
        if index == self.selected {
            return;
        }
        self.selected = index;
        // A deep screen is now showing a different plan — reset the per-plan
        // view state so we don't keep the previous plan's scroll position.
        self.replay_scroll = 0;
        self.popup_scroll = 0;
        if self.screen != Screen::List {
            self.record_depth(self.screen.depth());
            self.start_screen_jobs(self.screen);
        }
    }

    // -- search ------------------------------------------------------------

    /// Open the `/` prompt (list screen only). Filtering only makes sense over
    /// the picker; deeper screens are about one already-chosen plan.
    pub fn open_search(&mut self) {
        if self.screen != Screen::List {
            return;
        }
        self.search.open();
        self.status = Status::info("search: type to filter, Enter to keep, Esc to clear");
    }

    /// Type a character into the query and re-clamp the selection — the match
    /// set shrinks as you type, so the old index can fall off the end.
    pub fn search_push(&mut self, c: char) {
        self.search.push(c);
        self.clamp_selection();
    }

    /// Backspace one character (the match set grows; still re-clamp for safety).
    pub fn search_pop(&mut self) {
        self.search.pop();
        self.clamp_selection();
    }

    /// Close the prompt but keep the filter applied (Enter).
    pub fn search_accept(&mut self) {
        self.search.accept();
        self.clamp_selection();
        let n = self.visible().len();
        self.status = if self.search.is_filtering() {
            Status::info(format!(
                "filter '{}' — {n} plan(s); Esc or / to change",
                self.search.query
            ))
        } else {
            Status::info("search cleared")
        };
    }

    /// Close the prompt and drop the filter (Esc). The selection follows the
    /// currently-highlighted plan back into the unfiltered list, so clearing the
    /// filter doesn't jump you somewhere unrelated.
    pub fn search_clear(&mut self) {
        let keep = self.current_plan().map(|p| p.trade_id.clone());
        self.search.clear();
        self.selected = keep
            .and_then(|id| self.plans.iter().position(|p| p.trade_id == id))
            .unwrap_or(0);
        self.status = Status::info("search cleared");
    }

    /// Keep `selected` inside the visible range after the match set changes.
    fn clamp_selection(&mut self) {
        let n = self.visible().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
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
        if let Some(trade_id) = self.current_plan().map(|p| p.trade_id.clone()) {
            let entry = self.data.entry(trade_id).or_default();
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
            // NO auto TV-load here (removed 2026-07-27, operator's call): just
            // walking into a plan shouldn't yank the live chart around. Press
            // `l` to load it. The replay still loads the chart on demand,
            // because it re-arms FROM the chart (see `start_replay`).
            Screen::Timeline => self.start_timeline(&trade_id),
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

    /// Spawn the replay job unless cached or running. Replay re-arms from the
    /// live TradingView chart (`tv-arm --start <armed_at> replay`), so it needs
    /// the plan's `armed_at` (from the detail) as the `--start` cursor AND the
    /// chart loaded to this plan. The detail is fetched by the timeline job; if
    /// it isn't cached yet, kick that and let the retry (on re-enter) run the
    /// replay once it lands. Since screen entry no longer auto-loads the chart,
    /// the replay loads it itself when needed — that's not an "auto-load" of
    /// convenience, it's a hard precondition: tv-arm re-arms from whatever chart
    /// is up, so replaying against another plan's chart would give a wrong
    /// answer rather than just being slow.
    fn start_replay(&mut self, trade_id: &str) {
        let cached = self
            .data
            .get(trade_id)
            .map(|d| d.replay_report.is_some())
            .unwrap_or(false);
        if cached {
            return;
        }
        let armed_at = self
            .data
            .get(trade_id)
            .and_then(|d| d.detail.as_ref())
            .and_then(|d| d.armed_at.clone());
        let Some(armed_at) = armed_at else {
            // Detail (with armed_at) not loaded yet — fetch it; the replay is
            // retried when we re-enter/refresh once it's cached.
            self.start_timeline(trade_id);
            return;
        };
        // Replay re-arms from the live chart, so the chart must be loaded to this
        // plan first. If it isn't, kick the load and defer — the LoadTv
        // completion (apply_job) re-triggers the replay once the chart is up.
        let tv_loaded = self
            .data
            .get(trade_id)
            .map(|d| d.tv_loaded)
            .unwrap_or(false);
        if !tv_loaded {
            self.start_load_tv(trade_id);
            self.status = Status::info(format!("{trade_id}: loading chart before replay…"));
            return;
        }
        if !self.mark_in_flight(trade_id, JobKind::Replay) {
            return;
        }
        // Reproduce the ORIGINAL plan's prep set when re-arming from the chart:
        // a skip-BCR plan must replay with the skip flags, or tv-arm re-arms with
        // the full break-and-close-then-retest and stalls in AwaitBreakAndClose.
        let skip_flags: Vec<String> = self
            .data
            .get(trade_id)
            .and_then(|d| d.detail.as_ref())
            .map(|d| {
                d.bcr_preps
                    .tv_arm_skip_flags()
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        self.status = Status::info(format!("{trade_id}: running replay…"));
        jobs::spawn_replay(
            self.job_tx.clone(),
            trade_id.to_string(),
            armed_at,
            skip_flags,
        );
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
                    // A TV-load or replay may have been ASKED FOR before the
                    // export existed (both need the detail: the broker for the
                    // exchange prefix, `armed_at` for the replay cursor), in
                    // which case they parked themselves as pending and kicked
                    // this timeline job. Now that the detail is here, run them.
                    // Nothing auto-loads that the operator didn't request.
                    if self.tv_load_pending {
                        self.tv_load_pending = false;
                        self.start_load_tv(&trade_id);
                    }
                    if matches!(self.screen, Screen::Replay | Screen::Compare) {
                        self.start_replay(&trade_id);
                    }
                }
            }
            JobOutcome::Replay(report) => {
                self.data.entry(trade_id.clone()).or_default().replay_report = Some(report);
                self.status = Status::info(format!("{trade_id}: replay done"));
            }
            JobOutcome::LoadTv { already_there } => {
                self.data.entry(trade_id.clone()).or_default().tv_loaded = true;
                // Distinguish the two, so the operator knows whether their
                // scroll position was preserved (already-there) or reset (load).
                self.status = Status::info(if already_there {
                    format!("{trade_id}: already on this chart")
                } else {
                    format!("{trade_id}: loaded in TradingView")
                });
                // Replay re-arms from the now-loaded chart. If we're waiting on
                // Replay/Compare for this plan, kick it now that the chart is up.
                let is_open = self
                    .current_plan()
                    .map(|p| p.trade_id == trade_id)
                    .unwrap_or(false);
                if is_open && matches!(self.screen, Screen::Replay | Screen::Compare) {
                    self.start_replay(&trade_id);
                }
                // A parked `s` capture was waiting on exactly this chart.
                if is_open && self.save_fixture_pending {
                    self.save_fixture_pending = false;
                    let armed_at = self
                        .data
                        .get(&trade_id)
                        .and_then(|d| d.detail.as_ref())
                        .and_then(|d| d.armed_at.clone());
                    match armed_at {
                        Some(a) => self.spawn_save_fixture(&trade_id, a),
                        None => {
                            self.status =
                                Status::error(format!("{trade_id}: no armed_at — cannot capture"))
                        }
                    }
                }
            }
            JobOutcome::SaveFixture(report) => {
                // Cache the capture report so the Replay screen can show it (it
                // ends with tv-arm's per-cell grid summary). A failed capture
                // arrives as `Failed` instead, so reaching here means success.
                self.data
                    .entry(trade_id.clone())
                    .or_default()
                    .fixture_report = Some(report);
                // The corpus just grew — re-scan so the info-bar indicator flips
                // to "saved" without restarting the TUI.
                self.fixtures = crate::fixtures::scan(&crate::fixtures::default_dir());
                // Report what actually landed rather than a hardcoded count: the
                // grid has grown from six cells to eight (a fourth entry rule,
                // `strategy-v2-qm-market`, joined the news on/off pair), and a
                // baked-in number silently goes stale the next time it changes.
                let cells = match self.current_fixture_status() {
                    crate::fixtures::Status::Saved { cells, .. } => format!("{cells} cells"),
                    crate::fixtures::Status::None => "saved".to_string(),
                };
                self.status = Status::info(format!(
                    "{trade_id}: fixtures in replay-fixtures/ ({cells})"
                ));
            }
            JobOutcome::Failed(msg) => {
                self.status = Status::error(format!("{trade_id} {}: {msg}", kind.verb()));
            }
        }
    }

    // -- actions -----------------------------------------------------------

    /// Load the current plan into TradingView (the `l` key) — set the live
    /// chart's symbol + timeframe for this setup (the operator scrolls/zooms to
    /// it manually), as a background job so the navigation doesn't freeze the
    /// UI. **Only ever operator-initiated**: `l`, or the replay needing the
    /// chart it re-arms from. Entering a screen never loads the chart.
    pub fn load_tv(&mut self) {
        let Some(trade_id) = self.current_plan().map(|p| p.trade_id.clone()) else {
            return;
        };
        self.start_load_tv(&trade_id);
    }

    /// Spawn the TradingView-load job for `trade_id` once the plan detail is
    /// loaded — the detail carries the `broker`, which fixes the chart's
    /// exchange prefix. If the detail isn't loaded yet, park the request
    /// (`tv_load_pending`) and kick the timeline job; `apply_job` runs the
    /// parked load when the detail lands. Only called from an operator action
    /// (`l`) or the replay, never on screen entry.
    fn start_load_tv(&mut self, trade_id: &str) {
        // Instrument + granularity come from the list row; broker from detail.
        let Some(row) = self.plans.iter().find(|p| p.trade_id == trade_id) else {
            return;
        };
        let instrument = row.instrument.clone();
        let granularity = row.granularity.clone();
        // Broker comes from the fetched detail (drives the exchange prefix).
        let Some(detail) = self.data.get(trade_id).and_then(|d| d.detail.as_ref()) else {
            // Detail (with the broker) not loaded yet — park this request and
            // fetch it; the Timeline completion runs the parked load. The flag
            // is what distinguishes "the operator asked" from "we happened to
            // load a timeline", now that screen entry no longer auto-loads.
            self.tv_load_pending = true;
            self.start_timeline(trade_id);
            return;
        };
        let broker = detail.broker.clone();
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
        );
    }

    /// Capture the six-cell fixture corpus for the current plan (the `s` key) —
    /// `tv-arm --save-fixture … replay`.
    ///
    /// Replaced the old "record the outcome to a SQLite journal" action
    /// (2026-07-29): a fixture pins the actual candles + expected outcome and can
    /// be re-run offline forever, which subsumes what a recorded row gave us.
    ///
    /// Shares the replay's hard precondition — tv-arm re-arms from whatever chart
    /// is up, so the plan's chart must be loaded first or the capture freezes the
    /// WRONG setup. Rather than refuse, this parks the request and drives the
    /// chart load itself, mirroring [`Self::start_replay`].
    pub fn save_fixture_current(&mut self) {
        let Some(trade_id) = self.current_plan().map(|p| p.trade_id.clone()) else {
            return;
        };
        let armed_at = self
            .data
            .get(&trade_id)
            .and_then(|d| d.detail.as_ref())
            .and_then(|d| d.armed_at.clone());
        let Some(armed_at) = armed_at else {
            // No detail yet (it carries `armed_at` and the broker). Park the
            // capture, and load the chart — which itself parks behind the
            // timeline fetch. Both land via `apply_job`.
            self.save_fixture_pending = true;
            self.status = Status::info(format!("{trade_id}: loading plan before capture…"));
            self.start_load_tv(&trade_id);
            return;
        };
        let tv_loaded = self
            .data
            .get(&trade_id)
            .map(|d| d.tv_loaded)
            .unwrap_or(false);
        if !tv_loaded {
            self.save_fixture_pending = true;
            self.status = Status::info(format!("{trade_id}: loading chart before capture…"));
            self.start_load_tv(&trade_id);
            return;
        }
        self.spawn_save_fixture(&trade_id, armed_at);
    }

    /// Spawn the capture job for a plan whose chart is already loaded. Split from
    /// [`Self::save_fixture_current`] so the deferred path (chart just finished
    /// loading) can reuse it without re-running the gates.
    fn spawn_save_fixture(&mut self, trade_id: &str, armed_at: String) {
        if !self.mark_in_flight(trade_id, JobKind::SaveFixture) {
            return;
        }
        // Same divergence guard as the replay: reproduce the ORIGINAL plan's prep
        // set, or the captured fixtures pin the wrong gates.
        let skip_flags: Vec<String> = self
            .data
            .get(trade_id)
            .and_then(|d| d.detail.as_ref())
            .map(|d| {
                d.bcr_preps
                    .tv_arm_skip_flags()
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        self.status = Status::info(format!("{trade_id}: saving fixtures…"));
        jobs::spawn_save_fixture(
            self.job_tx.clone(),
            trade_id.to_string(),
            armed_at,
            skip_flags,
            trade_id.to_string(),
        );
    }

    /// Request a replay re-run (the `r` key), bypassing the cache.
    pub fn rerun_replay(&mut self) {
        let Some(trade_id) = self.current_plan().map(|p| p.trade_id.clone()) else {
            return;
        };
        if let Some(d) = self.data.get_mut(&trade_id) {
            d.replay_report = None;
        }
        // A fresh report starts at the top.
        self.replay_scroll = 0;
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
                        // The deleted row leaves the (possibly filtered) list, so
                        // re-clamp against the VISIBLE count, not `plans.len()`.
                        self.clamp_selection();
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

    /// Scroll the Replay report by `delta` lines (negative = up), clamped at 0.
    /// The bottom is bounded by the render (won't scroll past the content).
    pub fn scroll_replay(&mut self, delta: i32) {
        let next = self.replay_scroll as i32 + delta;
        self.replay_scroll = next.max(0) as u16;
    }

    /// Jump the Replay report to the top.
    pub fn scroll_replay_home(&mut self) {
        self.replay_scroll = 0;
    }

    /// Jump the Replay report to the bottom; the renderer pins it to the last
    /// page (same convention as the popup End).
    pub fn scroll_replay_end(&mut self) {
        self.replay_scroll = u16::MAX;
    }

    /// Request a full-screen repaint on the next frame (the Ctrl-L refresh key).
    /// The event loop clears the terminal before drawing, recovering from any
    /// residual corruption (a stray escape from a subprocess, a resize artifact).
    pub fn request_redraw(&mut self) {
        self.needs_clear = true;
        self.status = Status::info("refreshed");
    }

    /// Copy the **full** content of the current view (the whole list / timeline /
    /// replay / compare, or the detail popup if open — not just the visible
    /// part) to the system clipboard (the `c` key).
    pub fn copy_current(&mut self) {
        let text = crate::content::current(self);
        let lines = text.lines().count();
        match crate::clipboard::copy(&text) {
            Ok(tool) => {
                self.status = Status::info(format!("copied {lines} line(s) to clipboard ({tool})"))
            }
            Err(e) => self.status = Status::error(format!("copy: {e}")),
        }
    }

    /// Open the current plan's arm-time TradingView screenshot in the browser
    /// (the `o` key) — the chart as the operator saw it when they armed.
    ///
    /// A plan with no screenshot says so in the status line rather than failing
    /// silently: `o` on a plan armed before the feature (or with nothing on the
    /// clipboard) should explain itself, not look broken.
    pub fn open_screenshot(&mut self) {
        let Some(url) = self.current_screenshot_url() else {
            self.status = Status::info("no screenshot URL on this plan");
            return;
        };
        match crate::opener::open(&url) {
            Ok(tool) => self.status = Status::info(format!("opened screenshot ({tool})")),
            Err(e) => self.status = Status::error(format!("open screenshot: {e}")),
        }
    }

    /// The current plan's screenshot URL as an owned string, if it has one.
    /// Owned so the caller can mutate `self.status` without holding a borrow.
    fn current_screenshot_url(&self) -> Option<String> {
        let plan = self.current_plan()?;
        Some(
            self.data
                .get(&plan.trade_id)?
                .detail
                .as_ref()?
                .screenshot_url
                .as_ref()?
                .to_string(),
        )
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
            replay_scroll: 0,
            needs_clear: false,
            search: SearchState::default(),
            tv_load_pending: false,
            save_fixture_pending: false,
            // Render tests must not depend on whatever is on the developer's
            // disk; a test that wants a corpus sets `fixtures` explicitly.
            fixtures: Vec::new(),
        }
    }

    /// Seed the current plan's cached data (detail + timeline) so deeper-screen
    /// render tests have something to draw.
    pub fn seed_current(&mut self, data: PlanData) {
        if let Some(trade_id) = self.current_plan().map(|p| p.trade_id.clone()) {
            self.data.insert(trade_id, data);
        }
    }

    /// Force the visible screen (test helper).
    pub fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
    }

    /// Move the selection to the plan with the given trade_id (test helper).
    /// `selected` is in visible-space, so the position is looked up there.
    pub fn select_to(&mut self, trade_id: &str) {
        let pos = self
            .visible()
            .into_iter()
            .position(|i| self.plans.get(i).map(|p| p.trade_id.as_str()) == Some(trade_id));
        if let Some(i) = pos {
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

    /// Whether a fixture capture is parked waiting on the chart (test helper).
    pub fn save_fixture_pending(&self) -> bool {
        self.save_fixture_pending
    }

    /// Whether a specific job is in flight (test helper).
    pub fn in_flight_test(&self, trade_id: &str, kind: JobKind) -> bool {
        self.in_flight.contains(&(trade_id.to_string(), kind))
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

    /// The capture re-arms from the live chart, so pressing `s` with the chart
    /// NOT loaded must not fire tv-arm against whatever chart happens to be up —
    /// it parks the request and loads the chart first.
    #[test]
    fn save_fixture_defers_until_the_chart_is_loaded() {
        let mut app = App::from_rows(vec![row("hs-aud-cad-a07622da")]);
        app.seed_current(PlanData {
            detail: parse_plan_export(EXPORT).ok(),
            export_json: Some(EXPORT.to_string()),
            timeline_json: Some(TIMELINE.to_string()),
            replay_report: None,
            tv_loaded: false, // chart NOT up
            max_depth: 1,
            fixture_report: None,
        });
        app.save_fixture_current();
        assert!(app.save_fixture_pending(), "capture parked");
        assert!(
            !app.in_flight_test("hs-aud-cad-a07622da", JobKind::SaveFixture),
            "must NOT capture against an unloaded chart"
        );
    }

    /// With the chart loaded, `s` spawns the capture immediately.
    #[test]
    fn save_fixture_runs_when_the_chart_is_loaded() {
        let mut app = App::from_rows(vec![row("hs-aud-cad-a07622da")]);
        app.seed_current(PlanData {
            detail: parse_plan_export(EXPORT).ok(),
            export_json: Some(EXPORT.to_string()),
            timeline_json: Some(TIMELINE.to_string()),
            replay_report: Some(REPLAY.to_string()),
            tv_loaded: true,
            max_depth: 3,
            fixture_report: None,
        });
        app.save_fixture_current();
        assert!(
            app.in_flight_test("hs-aud-cad-a07622da", JobKind::SaveFixture),
            "capture spawned: {}",
            app.status.text
        );
        assert!(!app.save_fixture_pending(), "not parked — it ran");
    }

    /// A finished capture caches its report and reports success.
    #[test]
    fn save_fixture_completion_caches_the_report() {
        let mut app = App::from_rows(vec![row("t1")]);
        app.mark_in_flight_test("t1", JobKind::SaveFixture);
        app.inject_job(JobResult {
            trade_id: "t1".into(),
            kind: JobKind::SaveFixture,
            outcome: JobOutcome::SaveFixture("saved 6 cells".into()),
        });
        app.drain_jobs();
        assert!(!app.status.is_error, "{}", app.status.text);
        assert_eq!(
            app.data.get("t1").and_then(|d| d.fixture_report.as_deref()),
            Some("saved 6 cells")
        );
        assert_eq!(app.in_flight_len(), 0);
    }

    /// A finished capture must re-scan the corpus, so the info-bar indicator
    /// flips from "no fixture" to a count without restarting the TUI. This is
    /// the day-to-day path: today every live plan reads "no fixture" (the
    /// corpus was captured from plans since deleted), so pressing `s` and
    /// seeing the row change is how the operator knows it worked.
    ///
    /// The re-scan reads the real corpus directory, so this asserts the *call
    /// happens* by pointing it at a temp dir holding one matching capture —
    /// not at whatever is on the developer's disk.
    #[test]
    fn a_finished_capture_rescans_so_the_indicator_updates() {
        let tmp =
            std::env::temp_dir().join(format!("journal-rescan-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&tmp);
        for cell in ["t1-normal-news-off", "t1-normal-news-on"] {
            let d = tmp.join(cell);
            std::fs::create_dir_all(&d).expect("create cell dir");
            std::fs::write(
                d.join("meta.json"),
                r#"{"instrument":"AUD_CAD","granularity":"h1","source":"oanda",
                    "start":"2026-07-22T10:00:00Z","end":"2026-07-24T06:00:00Z"}"#,
            )
            .expect("write meta");
        }
        // SAFETY: single-threaded test; the override is read by `default_dir`
        // on the next scan and removed before returning.
        unsafe { std::env::set_var("TRADE_CONTROL_FIXTURES_DIR", &tmp) };

        let mut app = App::from_rows(vec![row("t1")]);
        app.seed_current(PlanData {
            detail: parse_plan_export(EXPORT).ok(),
            export_json: Some(EXPORT.to_string()),
            timeline_json: Some(TIMELINE.to_string()),
            replay_report: None,
            tv_loaded: true,
            max_depth: 1,
            fixture_report: None,
        });
        // The plan row must carry the fixture's instrument/granularity for the
        // match; `row()` already uses AUD_CAD / h1, and EXPORT's armed_at is
        // 2026-07-22T09:12Z — inside the window of the cells written above.
        assert!(
            !app.current_fixture_status().is_saved(),
            "starts with an empty corpus"
        );

        app.mark_in_flight_test("t1", JobKind::SaveFixture);
        app.inject_job(JobResult {
            trade_id: "t1".into(),
            kind: JobKind::SaveFixture,
            outcome: JobOutcome::SaveFixture("captured".into()),
        });
        app.drain_jobs();

        let status = app.current_fixture_status();
        assert!(
            status.is_saved(),
            "the completed capture must re-scan: {status:?}"
        );
        assert_eq!(status.label(), "fixture 2 ✓");
        // The status line reports what actually landed, not a hardcoded count.
        assert!(
            app.status.text.contains("2 cells"),
            "status names the real count: {}",
            app.status.text
        );

        unsafe { std::env::remove_var("TRADE_CONTROL_FIXTURES_DIR") };
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Filtering must move the SELECTION with the rows, not just hide rows: the
    /// highlight is an index into the visible list, so after a filter the
    /// selected plan must be one that actually matches.
    #[test]
    fn filter_keeps_selection_on_a_visible_plan() {
        let mut app = App::from_rows(vec![row("hs-eur-usd-1"), row("hs-aud-cad-2")]);
        // Make the two rows distinguishable by instrument.
        app.plans[0].instrument = "EUR_USD".into();
        app.plans[1].instrument = "AUD_CAD".into();

        app.selected = 1; // AUD_CAD
        assert_eq!(
            app.current_plan().map(|p| p.trade_id.as_str()),
            Some("hs-aud-cad-2")
        );

        // Filter to EUR: only one row is visible, and the out-of-range selection
        // clamps onto it rather than dangling past the end.
        app.open_search();
        for c in "eur".chars() {
            app.search_push(c);
        }
        assert_eq!(app.visible().len(), 1);
        assert_eq!(
            app.current_plan().map(|p| p.trade_id.as_str()),
            Some("hs-eur-usd-1"),
            "selection follows the filter"
        );
    }

    /// Navigation wraps over the VISIBLE rows, not the full list — otherwise
    /// `↓` would walk into filtered-out plans.
    #[test]
    fn navigation_wraps_within_the_filtered_set() {
        let mut app = App::from_rows(vec![row("a"), row("b"), row("c")]);
        app.plans[0].instrument = "EUR_USD".into();
        app.plans[1].instrument = "AUD_CAD".into();
        app.plans[2].instrument = "EUR_GBP".into();

        app.open_search();
        for c in "eur".chars() {
            app.search_push(c);
        }
        assert_eq!(app.visible().len(), 2, "two EUR plans match");

        assert_eq!(app.current_plan().map(|p| p.trade_id.as_str()), Some("a"));
        app.select_next();
        assert_eq!(
            app.current_plan().map(|p| p.trade_id.as_str()),
            Some("c"),
            "skips the filtered-out AUD_CAD"
        );
        app.select_next();
        assert_eq!(
            app.current_plan().map(|p| p.trade_id.as_str()),
            Some("a"),
            "wraps at the end of the FILTERED set"
        );
    }

    /// Clearing the filter (Esc) keeps you on the plan you had highlighted,
    /// rather than jumping to whatever now sits at that index.
    #[test]
    fn clearing_the_filter_keeps_the_same_plan_selected() {
        let mut app = App::from_rows(vec![row("a"), row("b"), row("c")]);
        app.plans[0].instrument = "EUR_USD".into();
        app.plans[1].instrument = "AUD_CAD".into();
        app.plans[2].instrument = "EUR_GBP".into();

        app.open_search();
        for c in "eur".chars() {
            app.search_push(c);
        }
        app.select_next(); // the second EUR plan, "c"
        assert_eq!(app.current_plan().map(|p| p.trade_id.as_str()), Some("c"));

        app.search_clear();
        assert_eq!(app.visible().len(), 3, "filter dropped");
        assert_eq!(
            app.current_plan().map(|p| p.trade_id.as_str()),
            Some("c"),
            "still on the same plan after clearing"
        );
    }

    /// A query matching nothing shows nothing and has no current plan — the
    /// actions that need one must no-op rather than panic or act on a stale row.
    #[test]
    fn no_match_leaves_no_current_plan() {
        let mut app = App::from_rows(vec![row("a")]);
        app.open_search();
        for c in "zzz".chars() {
            app.search_push(c);
        }
        assert!(app.visible().is_empty());
        assert!(app.current_plan().is_none());
        // Actions that read the current plan are safe no-ops.
        app.push_deeper();
        assert_eq!(app.screen, Screen::List, "cannot drill into nothing");
        app.request_delete();
        assert!(app.confirm.is_none(), "no delete confirm without a plan");
    }

    /// The prompt only opens on the list — deeper screens are about one plan.
    #[test]
    fn search_does_not_open_on_deeper_screens() {
        let mut app = App::from_rows(vec![row("a")]);
        app.set_screen(Screen::Replay);
        app.open_search();
        assert!(!app.search.active, "no search prompt off the list screen");
    }

    /// Moving the selection on a DEEP screen must fetch the newly-selected
    /// plan. Without this the timeline view sits on "loading timeline…" forever
    /// — nothing was loading, and `←` then `→` was the only way to trigger it.
    #[test]
    fn selecting_another_plan_on_a_deep_screen_fetches_it() {
        let mut app = App::from_rows(vec![row("t1"), row("t2")]);
        app.set_screen(Screen::Timeline);
        assert_eq!(app.in_flight_len(), 0);

        app.select_next(); // now showing t2, which has nothing cached
        assert_eq!(app.current_plan().map(|p| p.trade_id.as_str()), Some("t2"));
        assert!(
            app.in_flight.contains(&("t2".into(), JobKind::Timeline)),
            "the newly-selected plan's timeline must actually be fetched"
        );
    }

    /// The same move on the LIST fetches nothing — the list shows no per-plan
    /// data, so arrowing through the backlog must stay free.
    #[test]
    fn selecting_on_the_list_fetches_nothing() {
        let mut app = App::from_rows(vec![row("t1"), row("t2")]);
        app.select_next();
        assert_eq!(app.in_flight_len(), 0, "list navigation spawns no jobs");
    }

    /// Switching plans on a deep screen resets the per-plan scroll positions,
    /// so the new plan's report doesn't open scrolled to the old one's offset.
    #[test]
    fn switching_plans_resets_scroll() {
        let mut app = App::from_rows(vec![row("t1"), row("t2")]);
        app.set_screen(Screen::Timeline);
        app.scroll_replay(40);
        app.scroll_popup(15);
        app.select_next();
        assert_eq!(app.replay_scroll, 0);
        assert_eq!(app.popup_scroll, 0);
    }

    /// Walking into a plan must NOT touch the live TradingView chart — the
    /// operator drives that with `l`. Only the timeline job is spawned.
    #[test]
    fn entering_a_screen_does_not_load_tv() {
        let mut app = App::from_rows(vec![row("hs-aud-cad-a07622da")]);
        app.select_to("hs-aud-cad-a07622da");
        // Detail already cached, so nothing is waiting on a fetch: if an
        // auto-load existed, it would fire immediately here.
        app.seed_current(PlanData {
            detail: parse_plan_export(EXPORT).ok(),
            export_json: Some(EXPORT.to_string()),
            timeline_json: Some(TIMELINE.to_string()),
            replay_report: None,
            tv_loaded: false,
            max_depth: 0,
            fixture_report: None,
        });
        app.push_deeper(); // List → Timeline
        assert_eq!(app.screen, Screen::Timeline);
        assert!(
            !app.in_flight
                .contains(&("hs-aud-cad-a07622da".into(), JobKind::LoadTv)),
            "entering Timeline must not load the chart"
        );
    }

    /// `l` still loads it, even when the detail hasn't arrived yet: the request
    /// parks and runs when the timeline job lands. This is the path that would
    /// silently break if the pending flag were dropped along with the auto-load.
    #[test]
    fn pressing_l_before_detail_loads_still_loads_tv() {
        let mut app = App::from_rows(vec![row("hs-aud-cad-a07622da")]);
        app.select_to("hs-aud-cad-a07622da");
        app.set_screen(Screen::Timeline);

        app.load_tv(); // no detail cached yet → parks behind the timeline fetch
        assert!(
            !app.in_flight
                .contains(&("hs-aud-cad-a07622da".into(), JobKind::LoadTv)),
            "cannot load before the broker is known"
        );

        // The timeline job lands, carrying the detail (and the broker).
        app.in_flight
            .remove(&("hs-aud-cad-a07622da".into(), JobKind::Timeline));
        app.inject_job(JobResult {
            trade_id: "hs-aud-cad-a07622da".into(),
            kind: JobKind::Timeline,
            outcome: JobOutcome::Timeline {
                export_json: EXPORT.to_string(),
                timeline_json: TIMELINE.to_string(),
            },
        });
        app.drain_jobs();
        assert!(
            app.in_flight
                .contains(&("hs-aud-cad-a07622da".into(), JobKind::LoadTv)),
            "the parked `l` request runs once the detail lands"
        );
    }

    /// The same timeline completion must NOT load the chart when the operator
    /// never asked — this is the auto-load, and it's gone.
    #[test]
    fn timeline_completion_alone_does_not_load_tv() {
        let mut app = App::from_rows(vec![row("hs-aud-cad-a07622da")]);
        app.select_to("hs-aud-cad-a07622da");
        app.set_screen(Screen::Timeline);
        app.inject_job(JobResult {
            trade_id: "hs-aud-cad-a07622da".into(),
            kind: JobKind::Timeline,
            outcome: JobOutcome::Timeline {
                export_json: EXPORT.to_string(),
                timeline_json: TIMELINE.to_string(),
            },
        });
        app.drain_jobs();
        assert!(
            !app.in_flight
                .contains(&("hs-aud-cad-a07622da".into(), JobKind::LoadTv)),
            "no `l` pressed → no chart load"
        );
    }

    /// Replay must not fire until the chart is loaded (it re-arms from the live
    /// chart). With the detail present but `tv_loaded` false, start_replay
    /// defers — it kicks the TV-load instead of marking a Replay job in-flight.
    #[test]
    fn replay_waits_for_tv_load() {
        let mut app = App::from_rows(vec![row("hs-aud-cad-a07622da")]);
        app.select_to("hs-aud-cad-a07622da");
        app.seed_current(PlanData {
            detail: parse_plan_export(EXPORT).ok(),
            export_json: Some(EXPORT.to_string()),
            timeline_json: Some(TIMELINE.to_string()),
            replay_report: None,
            tv_loaded: false, // chart not loaded yet
            max_depth: 2,
            fixture_report: None,
        });
        app.set_screen(Screen::Replay);
        app.start_replay("hs-aud-cad-a07622da");
        // No Replay job in-flight — it deferred behind the chart load.
        assert!(
            !app.in_flight
                .contains(&("hs-aud-cad-a07622da".into(), JobKind::Replay)),
            "replay must wait for the chart, not fire immediately"
        );
        // Once the chart loads, a subsequent start_replay proceeds (marks Replay
        // in-flight, since armed_at is present and tv_loaded is now true).
        app.data
            .get_mut("hs-aud-cad-a07622da")
            .expect("data")
            .tv_loaded = true;
        app.start_replay("hs-aud-cad-a07622da");
        assert!(
            app.in_flight
                .contains(&("hs-aud-cad-a07622da".into(), JobKind::Replay)),
            "with the chart loaded, replay fires"
        );
    }
}

#[cfg(test)]
mod e2e {
    //! End-to-end against the LIVE worker (ignored by default; run with
    //! `cargo test -p journal -- --ignored --nocapture`). Reproduces the
    //! operator's report: on the Timeline screen, `↓` used to sit on
    //! "loading timeline…" forever.
    use super::*;

    #[test]
    #[ignore]
    fn down_on_timeline_actually_loads_the_next_plan() {
        let mut app = App::new().expect("fetch plan list from the live worker");
        assert!(app.plans.len() > 1, "need 2+ plans to move between");

        app.push_deeper(); // List -> Timeline, fetches plan #1
        let first = app.current_plan().expect("plan 1").trade_id.clone();

        app.select_next(); // the reported keypress
        let second = app.current_plan().expect("plan 2").trade_id.clone();
        assert_ne!(first, second);

        // Drain until the newly-selected plan's timeline lands (or time out).
        let mut waited = 0;
        while app
            .current_data()
            .and_then(|d| d.timeline_json.as_ref())
            .is_none()
        {
            std::thread::sleep(std::time::Duration::from_millis(200));
            app.drain_jobs();
            waited += 1;
            assert!(waited < 100, "timeline for {second} never loaded (the bug)");
        }
        println!("OK: {second} loaded after ~{}ms of `↓` alone", waited * 200);
    }
}
