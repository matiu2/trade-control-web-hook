//! Report selection shared by the replay pane and clipboard.

use super::{App, ReportKind};

impl App {
    /// Which report the pane shows, and what to call it.
    ///
    /// Read from [`crate::app::PlanData::shown_report`], set by whichever job
    /// landed most recently — deliberately NOT a fixed precedence over the three
    /// `Option`s. A precedence looks equivalent and is not: with one, a re-bless
    /// would shadow every later `r` for the rest of the session, so the operator
    /// would re-run a replay and be shown the old re-bless under a confident
    /// title. Recording which one landed last is the only version that can be
    /// right in both directions.
    ///
    /// A selected report that has not run yet falls back to whatever HAS, so an
    /// empty pane only ever means "nothing has run for this plan".
    pub fn current_report(&self) -> (Option<&str>, &'static str) {
        let Some(d) = self.current_data() else {
            return (None, "Replay report");
        };
        let chosen = match d.shown_report {
            ReportKind::Rebless => (d.rebless_report.as_deref(), "Re-bless report"),
            ReportKind::RawReplay => (
                d.raw_replay_report.as_deref(),
                "Raw replay report (stored plan)",
            ),
            ReportKind::Replay => (d.replay_report.as_deref(), "Replay report (re-armed)"),
        };
        if chosen.0.is_some() {
            return chosen;
        }
        // Fall back to any report that exists, so the pane is never blank while
        // the plan holds something worth reading.
        for (body, label) in [
            (d.replay_report.as_deref(), "Replay report (re-armed)"),
            (
                d.raw_replay_report.as_deref(),
                "Raw replay report (stored plan)",
            ),
            (d.rebless_report.as_deref(), "Re-bless report"),
        ] {
            if body.is_some() {
                return (body, label);
            }
        }
        (None, "Replay report")
    }
}
