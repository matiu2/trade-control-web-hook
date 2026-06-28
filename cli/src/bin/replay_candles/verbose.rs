//! Per-bar state-delta trace for `--verbose` (a.k.a. `--all-events`).
//!
//! The engine's [`evaluate_plan`](trade_control_engine::evaluate_plan) advances
//! the [`PlanState`](trade_control_engine::PlanState) silently: most of what it
//! decides per bar — a phase transition, the `break-and-close` stamp, the
//! **retest lookback stamp** — leaves no fired intent and so never appears in the
//! normal report. The classic confusion is "the plan requires a `retest` prep but
//! I never saw one fire": retest isn't an emitted prep at all, it's a retroactive
//! `retest_seen_at` stamp the entry gate later reads (see the engine module doc).
//!
//! This module makes those invisible state changes visible. The replay loop
//! snapshots the state **before and after** each live tick and records a
//! [`BarTrace`] of what changed — the new phase, any newly-set timestamps, and
//! the rule ids that fired this bar. `--verbose` then interleaves these traces
//! with the fire report, so the operator can see *exactly* which bar stamped the
//! retest, when the spine advanced, and which bars passed silently.
//!
//! It is a pure diff of two [`PlanState`] snapshots — no engine change, no extra
//! evaluation. A bar where nothing changed and nothing fired yields a trace whose
//! [`BarTrace::is_quiet`] is true; the renderer can skip those to keep the output
//! legible, or show them under a future `--all-bars`.

use chrono::{DateTime, Utc};
use trade_control_engine::{Phase, PlanState};

use super::brisbane::bne;

/// What changed on one live bar: the spine phase after the tick, any phase
/// transition, the two lookback stamps if newly set this bar, and the rule ids
/// that fired. Computed by [`BarTrace::diff`] from the before/after [`PlanState`]
/// snapshots the replay loop already holds.
#[derive(Debug, Clone, PartialEq)]
pub struct BarTrace {
    /// Open-time of the live bar this trace describes (UTC; rendered Brisbane).
    pub bar: DateTime<Utc>,
    /// The spine phase *after* this tick.
    pub phase: Phase,
    /// `Some(prev)` when the phase changed this bar (prev → `phase`); `None` when
    /// it held steady.
    pub phase_from: Option<Phase>,
    /// Set when `break_close_at` went from unset to set on this bar — the
    /// break-and-close prep was satisfied here.
    pub break_close_stamped: bool,
    /// Set when `retest_seen_at` went from unset to set on this bar — a candle
    /// satisfied the retest trendline geometry here, arming the entry gate. This
    /// is the event the normal report can never show.
    pub retest_stamped: bool,
    /// Rule ids that fired on this bar (in fire order), for cross-referencing the
    /// trace against the fire report.
    pub fired_rules: Vec<String>,
}

impl BarTrace {
    /// Diff the state before/after one tick into a [`BarTrace`]. `fired_rules` is
    /// the ids the engine reported firing this bar (the caller pulls them from
    /// `PlanEval::fired`). The stamp flags are edge-triggered: `None → Some` only,
    /// so a stamp set on an earlier bar isn't re-reported.
    pub fn diff(
        bar: DateTime<Utc>,
        before: &PlanState,
        after: &PlanState,
        fired_rules: Vec<String>,
    ) -> Self {
        let phase_from = (before.phase != after.phase).then_some(before.phase);
        let break_close_stamped = before.break_close_at.is_none() && after.break_close_at.is_some();
        let retest_stamped = before.retest_seen_at.is_none() && after.retest_seen_at.is_some();
        BarTrace {
            bar,
            phase: after.phase,
            phase_from,
            break_close_stamped,
            retest_stamped,
            fired_rules,
        }
    }

    /// A bar with nothing worth showing: no phase change, no new stamp, no fire.
    /// The renderer skips these so `--verbose` reports only the bars where the
    /// engine actually did something.
    pub fn is_quiet(&self) -> bool {
        self.phase_from.is_none()
            && !self.break_close_stamped
            && !self.retest_stamped
            && self.fired_rules.is_empty()
    }

    /// Render this trace as one indented block under a `bar …` header. Returns
    /// the empty string for a quiet bar (so the caller can `push_str` blindly).
    pub fn render(&self) -> String {
        if self.is_quiet() {
            return String::new();
        }
        let mut out = format!("  bar {} phase={:?}\n", bne(self.bar), self.phase);
        if let Some(from) = self.phase_from {
            out.push_str(&format!("    phase {from:?}→{:?}\n", self.phase));
        }
        if self.break_close_stamped {
            out.push_str("    ✓ break-and-close stamped (spine → AwaitEntry)\n");
        }
        if self.retest_stamped {
            out.push_str("    ✓ retest stamped (entry gate now satisfied)\n");
        }
        for rule in &self.fired_rules {
            out.push_str(&format!("    → fired {rule}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 23, h, 0, 0).unwrap()
    }

    fn state(phase: Phase) -> PlanState {
        PlanState::seed(phase, at(23))
    }

    #[test]
    fn quiet_bar_renders_empty() {
        let s = state(Phase::AwaitEntry);
        let t = BarTrace::diff(at(16), &s, &s, Vec::new());
        assert!(t.is_quiet());
        assert_eq!(t.render(), "");
    }

    #[test]
    fn retest_stamp_is_edge_triggered_and_rendered() {
        let before = state(Phase::AwaitEntry);
        let mut after = before.clone();
        after.retest_seen_at = Some(at(16));
        let t = BarTrace::diff(at(16), &before, &after, Vec::new());
        assert!(t.retest_stamped);
        assert!(!t.is_quiet());
        assert!(t.render().contains("retest stamped"));

        // Already-set on the prior bar → not re-reported.
        let t2 = BarTrace::diff(at(17), &after, &after, Vec::new());
        assert!(!t2.retest_stamped);
        assert!(t2.is_quiet());
    }

    #[test]
    fn break_close_stamp_is_reported_once() {
        let before = state(Phase::AwaitBreakAndClose);
        let mut after = before.clone();
        after.break_close_at = Some(at(15));
        after.phase = Phase::AwaitEntry;
        let t = BarTrace::diff(at(15), &before, &after, Vec::new());
        assert!(t.break_close_stamped);
        assert_eq!(t.phase_from, Some(Phase::AwaitBreakAndClose));
        let r = t.render();
        assert!(r.contains("break-and-close stamped"));
        assert!(r.contains("AwaitBreakAndClose→AwaitEntry"));
    }

    #[test]
    fn phase_transition_to_done_is_shown_with_the_fire() {
        let before = state(Phase::AwaitEntry);
        let mut after = before.clone();
        after.phase = Phase::Done;
        let t = BarTrace::diff(at(18), &before, &after, vec!["05-enter".into()]);
        let r = t.render();
        assert!(r.contains("→ fired 05-enter"));
        assert!(r.contains("AwaitEntry→Done"));
    }

    #[test]
    fn fire_alone_is_not_quiet() {
        let s = state(Phase::AwaitEntry);
        let t = BarTrace::diff(at(22), &s, &s, vec!["01-veto-too-low".into()]);
        assert!(!t.is_quiet());
        assert!(t.render().contains("→ fired 01-veto-too-low"));
    }
}
