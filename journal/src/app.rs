//! App state + the transitions the event loop drives. Keeps all business logic
//! (what to fetch on a screen push, the delete guard) here so `main.rs` is a
//! thin render/input loop.

use std::collections::HashMap;

use color_eyre::eyre::Result;

use crate::cli;
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
}

impl App {
    /// Build the app, fetching the initial plan list.
    pub fn new() -> Result<Self> {
        let plans = fetch_plans()?;
        Ok(Self {
            plans,
            selected: 0,
            screen: Screen::List,
            data: HashMap::new(),
            status: Status::info("loaded plans"),
            show_popup: false,
            confirm: None,
            should_quit: false,
        })
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

    /// Push one screen deeper, running that screen's side-effect the first time
    /// it's reached for this plan (timeline+detail+TV load, replay run).
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
        if let Err(e) = self.run_screen_effect(next) {
            self.status = Status::error(format!("{next:?}: {e}"));
        }
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

    /// Fetch whatever a freshly-entered screen needs, caching it per plan.
    fn run_screen_effect(&mut self, screen: Screen) -> Result<()> {
        let Some(trade_id) = self.current_plan().map(|p| p.trade_id.clone()) else {
            return Ok(());
        };
        match screen {
            Screen::Timeline => {
                self.ensure_detail_and_timeline(&trade_id)?;
                // TV auto-load happens here via the replay annotate path in a
                // later slice; for now just note readiness.
                self.status = Status::info(format!("{trade_id}: timeline loaded"));
            }
            Screen::Replay => {
                self.ensure_replay(&trade_id)?;
            }
            Screen::Compare => {
                // Compare reuses timeline + replay already fetched; v2 adds diff.
                self.ensure_detail_and_timeline(&trade_id)?;
                self.ensure_replay(&trade_id)?;
            }
            Screen::List => {}
        }
        Ok(())
    }

    /// Fetch `plan export` (→ detail + popup dump) and `plan timeline` once.
    fn ensure_detail_and_timeline(&mut self, trade_id: &str) -> Result<()> {
        let need_export = self
            .data
            .get(trade_id)
            .map(|d| d.export_json.is_none())
            .unwrap_or(true);
        if need_export {
            let export = cli::plan_export_json(trade_id)?;
            let detail = parse_plan_export(&export).ok();
            let entry = self.data.entry(trade_id.to_string()).or_default();
            entry.export_json = Some(export);
            entry.detail = detail;
        }
        let need_timeline = self
            .data
            .get(trade_id)
            .map(|d| d.timeline_json.is_none())
            .unwrap_or(true);
        if need_timeline {
            let tl = cli::plan_timeline_json(trade_id)?;
            self.data
                .entry(trade_id.to_string())
                .or_default()
                .timeline_json = Some(tl);
        }
        Ok(())
    }

    /// Run the replay once and cache its report.
    fn ensure_replay(&mut self, trade_id: &str) -> Result<()> {
        let need = self
            .data
            .get(trade_id)
            .map(|d| d.replay_report.is_none())
            .unwrap_or(true);
        if !need {
            return Ok(());
        }
        // Replay needs the plan JSON on disk; write the export to a temp file.
        let export = match self.data.get(trade_id).and_then(|d| d.export_json.clone()) {
            Some(e) => e,
            None => {
                let e = cli::plan_export_json(trade_id)?;
                self.data
                    .entry(trade_id.to_string())
                    .or_default()
                    .export_json = Some(e.clone());
                e
            }
        };
        let path = std::env::temp_dir().join(format!("journal-replay-{trade_id}.json"));
        std::fs::write(&path, export)?;
        self.status = Status::info(format!("{trade_id}: running replay…"));
        let report = cli::replay(&path, false)?;
        self.data
            .entry(trade_id.to_string())
            .or_default()
            .replay_report = Some(report);
        self.status = Status::info(format!("{trade_id}: replay done"));
        Ok(())
    }

    // -- actions -----------------------------------------------------------

    /// Load the current plan into TradingView by replaying with `--annotate`,
    /// which draws the simulated positions onto the live chart via tv-mcp.
    pub fn load_tv(&mut self) {
        let Some(trade_id) = self.current_plan().map(|p| p.trade_id.clone()) else {
            return;
        };
        if let Err(e) = self.annotate_tv(&trade_id) {
            self.status = Status::error(format!("TV load: {e}"));
        } else {
            self.status = Status::info(format!("{trade_id}: drawn on TradingView"));
        }
    }

    fn annotate_tv(&mut self, trade_id: &str) -> Result<()> {
        let export = match self.data.get(trade_id).and_then(|d| d.export_json.clone()) {
            Some(e) => e,
            None => {
                let e = cli::plan_export_json(trade_id)?;
                self.data
                    .entry(trade_id.to_string())
                    .or_default()
                    .export_json = Some(e.clone());
                e
            }
        };
        let path = std::env::temp_dir().join(format!("journal-tv-{trade_id}.json"));
        std::fs::write(&path, export)?;
        cli::replay(&path, true)?;
        Ok(())
    }

    /// Request a replay re-run (the `r` key), bypassing the cache.
    pub fn rerun_replay(&mut self) {
        let Some(trade_id) = self.current_plan().map(|p| p.trade_id.clone()) else {
            return;
        };
        if let Some(d) = self.data.get_mut(&trade_id) {
            d.replay_report = None;
        }
        if let Err(e) = self.ensure_replay(&trade_id) {
            self.status = Status::error(format!("replay: {e}"));
        }
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
        Self {
            plans,
            selected: 0,
            screen: Screen::List,
            data: HashMap::new(),
            status: Status::info("test"),
            show_popup: false,
            confirm: None,
            should_quit: false,
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
}
