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
use trade_control_engine::intent::{Direction, SignalKind};
use trade_control_engine::{Phase, PlanState};

use super::brisbane::bne;

/// Render a UTC instant as `YYYY-MM-DD HH:MM` for the spread-block window. UTC
/// (not Brisbane like the bar headers) because the spread-hour mask is defined
/// in UTC hours — showing the block in UTC keeps it aligned with the baked mask
/// the operator is calibrating.
fn utc_hm(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M").to_string()
}

/// A pattern the signal detector printed on a bar, whether or not the plan acted
/// on it. Attached to a [`BarTrace`] when the bar's detected signal passes the
/// active [`DetectorMarkConfig`](trade_control_cli::replay_args::DetectorMarkConfig)
/// filter. Computed with the SAME `detect_at` + `wilder_atr` the engine uses, so
/// a marked golden is exactly what the engine detected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectedMark {
    pub direction: Direction,
    pub kind: SignalKind,
    /// The size compared against ATR for the golden test (`Detected::size`).
    pub size: f64,
    /// The Wilder ATR at this bar, when warm — `None` if the window is too short
    /// to size ATR (then `golden` is necessarily false, same as the engine).
    pub atr: Option<f64>,
    /// `size >= atr` — the golden flag, exactly as the engine latches it.
    pub golden: bool,
}

impl DetectedMark {
    /// One-line detail for the `--verbose` bar block.
    fn render(&self) -> String {
        let tag = if self.golden { "GOLDEN" } else { "signal" };
        let atr = self
            .atr
            .map(|a| format!("{a:.5}"))
            .unwrap_or_else(|| "na".to_string());
        let size = self.size;
        format!(
            "    ◆ {tag} {:?} {:?} (size={size:.5} atr={atr})\n",
            self.direction, self.kind
        )
    }
}

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
    /// A detected signal on this bar that passed the active detector-mark filter
    /// (`--candle-detector-*`), whether or not the plan acted on it. `None` when
    /// no signal printed, the signal didn't pass the filter, or the feature is
    /// off. This is the "golden candle we never entered on" surface.
    pub detected: Option<DetectedMark>,
    /// Why a `PinePattern` enter *declined* this bar, when it did: a signal that
    /// fired and matched direction, but the pre-flight rejected it (needs-golden,
    /// needs-confirmed, or resolve-failed like below-min-R). Empty on bars where
    /// no enter was even evaluated or one fired cleanly. Paired with `detected`,
    /// this turns "golden seen but no entry" into a one-line answer.
    pub entry_declines: Vec<String>,
    /// Why a *marked* signal wasn't taken when its enter never even fired —
    /// blocked by an unmet precondition (break-and-close outstanding, no
    /// confirmed signal yet, retest unstamped). Distinct from `entry_declines`,
    /// which is the fired-then-declined case. Only set on a marked bar with no
    /// fire and no decline; `None` otherwise.
    pub not_taken: Option<String>,
    /// This bar is a learned **spread hour** for the instrument (per-instrument
    /// baked mask + 30-min lead, or the NY-close-edge fallback) — a rubbish
    /// liquidity-vacuum candle. The engine suppresses entries, signal detection,
    /// and level crosses on it (shared with the worker), so a golden that prints
    /// here (still marked in the detector summary) does NOT fire. Rendered so the
    /// "golden but no fire" isn't a silent mystery.
    pub spread_hour: bool,
    /// The `(start, end)` UTC bounds of the spread-hour block covering this bar,
    /// when `spread_hour` is set — half-open `[start, end)`, hour-snapped. Shown
    /// in the spread-hour line so the operator sees exactly how long the block
    /// runs ("spread-hour 21:00 → 06:00 UTC"). `None` when this isn't a spread
    /// hour (or the window couldn't be resolved).
    pub spread_block: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// A confirmed signal (matching the plan direction) exists as of this bar, so
    /// a confirmation-gated (QM) enter WOULD have been eligible — but this bar is
    /// a spread hour, so the engine suppressed the entry. This is the exact
    /// "10:00 bar confirmed, not entering due to the spread hour" case: without
    /// it, a confirmed setup whose confirmation lands inside a spread block reads
    /// as a plain suppressed golden with no hint that it was ready to enter.
    pub confirmed_while_suppressed: bool,
}

impl BarTrace {
    /// Diff the state before/after one tick into a [`BarTrace`]. `fired_rules` is
    /// the ids the engine reported firing this bar (the caller pulls them from
    /// `PlanEval::fired`). The stamp flags are edge-triggered: `None → Some` only,
    /// so a stamp set on an earlier bar isn't re-reported.
    #[allow(clippy::too_many_arguments)]
    pub fn diff(
        bar: DateTime<Utc>,
        before: &PlanState,
        after: &PlanState,
        fired_rules: Vec<String>,
        detected: Option<DetectedMark>,
        entry_declines: Vec<String>,
        not_taken: Option<String>,
        spread_hour: bool,
        spread_block: Option<(DateTime<Utc>, DateTime<Utc>)>,
        confirmed_while_suppressed: bool,
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
            detected,
            entry_declines,
            not_taken,
            spread_hour,
            spread_block,
            confirmed_while_suppressed,
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
            && self.detected.is_none()
            && self.entry_declines.is_empty()
            && self.not_taken.is_none()
            && !self.confirmed_while_suppressed
    }

    /// Render this trace as one indented block under a `bar …` header, with the
    /// replay report's **rich per-fire notes** for this bar (`placed` / `BLOCKED`
    /// / `FILLED` / exit / `pause` / `news-start` …) injected in place of the bare
    /// `→ fired <rule>` lines. `notes` comes from the same event stream the second
    /// section prints, bucketed by bar time (see `report::render`). When `notes`
    /// is empty the output is identical to the bare-fire form, so a
    /// `--simulate`-off run — or a bar that only carries a forward fill note —
    /// reads exactly as before. Returns the empty string for a quiet bar with no
    /// notes (so the caller can `push_str` blindly).
    ///
    /// A bar with injected notes but no other state change is **not quiet**: the
    /// notes are the whole point, so it renders.
    pub fn render_with_notes(&self, notes: &[String]) -> String {
        if self.is_quiet() && notes.is_empty() {
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
        if let Some(mark) = &self.detected {
            out.push_str(&mark.render());
        }
        // A spread-hour bar is rubbish: the engine suppressed entries, signal
        // detection, and level crosses on it.
        //
        // Two surfaces, both showing the block bounds:
        // - `confirmed_while_suppressed`: a confirmed signal became ready to enter
        //   on this suppressed bar (the QM enter would have fired but for the
        //   spread hour). This is the "10:00 setup confirmed, not entering because
        //   the spread hour runs X→Y" case the operator asked for. Shown even
        //   without a fresh mark on this bar, because confirmation lands a couple
        //   of bars after the golden printed.
        // - a fresh MARK on a spread-hour bar (golden printed but suppressed):
        //   the "golden printed and nothing fired" case that would otherwise
        //   mystify. Only shown when a mark is present (else every spread hour
        //   would add noise).
        if self.spread_hour && (self.confirmed_while_suppressed || self.detected.is_some()) {
            let window = self
                .spread_block
                .map(|(s, e)| format!(" {} → {} UTC", utc_hm(s), utc_hm(e)))
                .unwrap_or_default();
            if self.confirmed_while_suppressed {
                out.push_str(&format!(
                    "    ⌀ spread-hour{window} — signal confirmed, not entering (spread-hour suppressed)\n"
                ));
            } else {
                out.push_str(&format!(
                    "    ⌀ spread-hour{window} (rubbish candle) — entry/detection/crosses suppressed\n"
                ));
            }
        }
        for reason in &self.entry_declines {
            out.push_str(&format!("    ✗ not entered: {reason}\n"));
        }
        if let Some(reason) = &self.not_taken {
            out.push_str(&format!("    ✗ not taken: {reason}\n"));
        }
        // Fire lines: prefer the report's rich per-fire notes for this bar
        // (`entry #1 placed — order …`, `… BLOCKED — rejected: market-blackout …`,
        // `NEWS START — watching for reversal candles …`). They're the same lines
        // the second section prints, so the trace no longer needs the operator to
        // cross-reference. When no notes were injected (e.g. `--simulate` off),
        // fall back to the bare rule ids so the trace is never emptier than before.
        if notes.is_empty() {
            for rule in &self.fired_rules {
                out.push_str(&format!("    → fired {rule}\n"));
            }
        } else {
            for note in notes {
                out.push_str(&format!("    → {note}\n"));
            }
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
        let t = BarTrace::diff(
            at(16),
            &s,
            &s,
            Vec::new(),
            None,
            Vec::new(),
            None,
            false,
            None,
            false,
        );
        assert!(t.is_quiet());
        assert_eq!(t.render_with_notes(&[]), "");
    }

    #[test]
    fn retest_stamp_is_edge_triggered_and_rendered() {
        let before = state(Phase::AwaitEntry);
        let mut after = before.clone();
        after.retest_seen_at = Some(at(16));
        let t = BarTrace::diff(
            at(16),
            &before,
            &after,
            Vec::new(),
            None,
            Vec::new(),
            None,
            false,
            None,
            false,
        );
        assert!(t.retest_stamped);
        assert!(!t.is_quiet());
        assert!(t.render_with_notes(&[]).contains("retest stamped"));

        // Already-set on the prior bar → not re-reported.
        let t2 = BarTrace::diff(
            at(17),
            &after,
            &after,
            Vec::new(),
            None,
            Vec::new(),
            None,
            false,
            None,
            false,
        );
        assert!(!t2.retest_stamped);
        assert!(t2.is_quiet());
    }

    #[test]
    fn break_close_stamp_is_reported_once() {
        let before = state(Phase::AwaitBreakAndClose);
        let mut after = before.clone();
        after.break_close_at = Some(at(15));
        after.phase = Phase::AwaitEntry;
        let t = BarTrace::diff(
            at(15),
            &before,
            &after,
            Vec::new(),
            None,
            Vec::new(),
            None,
            false,
            None,
            false,
        );
        assert!(t.break_close_stamped);
        assert_eq!(t.phase_from, Some(Phase::AwaitBreakAndClose));
        let r = t.render_with_notes(&[]);
        assert!(r.contains("break-and-close stamped"));
        assert!(r.contains("AwaitBreakAndClose→AwaitEntry"));
    }

    #[test]
    fn phase_transition_to_done_is_shown_with_the_fire() {
        let before = state(Phase::AwaitEntry);
        let mut after = before.clone();
        after.phase = Phase::Done;
        let t = BarTrace::diff(
            at(18),
            &before,
            &after,
            vec!["05-enter".into()],
            None,
            Vec::new(),
            None,
            false,
            None,
            false,
        );
        let r = t.render_with_notes(&[]);
        assert!(r.contains("→ fired 05-enter"));
        assert!(r.contains("AwaitEntry→Done"));
    }

    #[test]
    fn fire_alone_is_not_quiet() {
        let s = state(Phase::AwaitEntry);
        let t = BarTrace::diff(
            at(22),
            &s,
            &s,
            vec!["01-veto-too-low".into()],
            None,
            Vec::new(),
            None,
            false,
            None,
            false,
        );
        assert!(!t.is_quiet());
        assert!(t.render_with_notes(&[]).contains("→ fired 01-veto-too-low"));
    }

    /// Injected rich notes REPLACE the bare `→ fired <rule>` lines: the trace
    /// shows the report's own "entry #1 placed …" / "… BLOCKED …" wording, so an
    /// operator no longer has to cross-reference the second section.
    #[test]
    fn injected_notes_replace_bare_fire_lines() {
        let s = state(Phase::AwaitEntry);
        let t = BarTrace::diff(
            at(3),
            &s,
            &s,
            vec!["09-enter-qm".into()],
            None,
            Vec::new(),
            None,
            false,
            None,
            false,
        );
        let notes = vec![
            "entry #1 placed — order UNRESOLVED".to_string(),
            "entry #1 BLOCKED — rejected: market-blackout → NO FILL / 0R".to_string(),
        ];
        let r = t.render_with_notes(&notes);
        // The rich notes are shown…
        assert!(r.contains("→ entry #1 placed — order UNRESOLVED"), "{r}");
        assert!(
            r.contains("→ entry #1 BLOCKED — rejected: market-blackout"),
            "{r}"
        );
        // …and the bare `→ fired 09-enter-qm` line is NOT (replaced, not appended).
        assert!(
            !r.contains("→ fired 09-enter-qm"),
            "bare fire line suppressed: {r}"
        );
    }

    /// A bar that only carries injected notes (no phase move / stamp / mark) is
    /// NOT quiet — the notes are the whole point (e.g. a forward bar where a fill
    /// landed).
    #[test]
    fn a_note_only_bar_is_not_quiet_and_renders() {
        let s = state(Phase::AwaitEntry);
        let t = BarTrace::diff(
            at(5),
            &s,
            &s,
            Vec::new(),
            None,
            Vec::new(),
            None,
            false,
            None,
            false,
        );
        // Quiet with no notes…
        assert!(t.is_quiet());
        assert_eq!(t.render_with_notes(&[]), "");
        // …but a single injected note makes it render.
        let notes = vec!["entry #1 FILLED @ 1.14282".to_string()];
        let r = t.render_with_notes(&notes);
        assert!(!r.is_empty());
        assert!(r.contains("→ entry #1 FILLED @ 1.14282"), "{r}");
    }

    /// A golden mark on a bar where nothing else happened is NOT quiet and
    /// renders the ◆ GOLDEN line — this is the "golden candle we never entered
    /// on" surface, visible even when no rule fired.
    #[test]
    fn golden_mark_alone_is_not_quiet_and_renders() {
        let s = state(Phase::AwaitEntry);
        let mark = DetectedMark {
            direction: Direction::Long,
            kind: SignalKind::Pinbar,
            size: 0.0042,
            atr: Some(0.0031),
            golden: true,
        };
        let t = BarTrace::diff(
            at(16),
            &s,
            &s,
            Vec::new(),
            Some(mark),
            Vec::new(),
            None,
            false,
            None,
            false,
        );
        assert!(!t.is_quiet());
        let r = t.render_with_notes(&[]);
        assert!(r.contains("◆ GOLDEN"), "golden mark rendered: {r}");
        assert!(r.contains("Long"), "direction shown: {r}");
    }

    /// A non-golden mark renders as `signal`, not `GOLDEN`.
    #[test]
    fn non_golden_mark_renders_as_signal() {
        let s = state(Phase::AwaitEntry);
        let mark = DetectedMark {
            direction: Direction::Short,
            kind: SignalKind::Tweezer,
            size: 0.0010,
            atr: Some(0.0031),
            golden: false,
        };
        let t = BarTrace::diff(
            at(16),
            &s,
            &s,
            Vec::new(),
            Some(mark),
            Vec::new(),
            None,
            false,
            None,
            false,
        );
        let r = t.render_with_notes(&[]);
        assert!(r.contains("◆ signal"), "non-golden mark: {r}");
        assert!(!r.contains("GOLDEN"), "not tagged golden: {r}");
    }

    /// A golden marked on a SPREAD-HOUR bar renders both the ◆ mark AND the
    /// `⌀ spread-hour` line, so a golden that printed but didn't fire is
    /// explained rather than silently missing.
    #[test]
    fn spread_hour_mark_renders_the_rubbish_note() {
        let s = state(Phase::AwaitEntry);
        let mark = DetectedMark {
            direction: Direction::Short,
            kind: SignalKind::Pinbar,
            size: 0.0006,
            atr: Some(0.0005),
            golden: true,
        };
        let t = BarTrace::diff(
            at(21),
            &s,
            &s,
            Vec::new(),
            Some(mark),
            Vec::new(),
            None,
            true,
            Some((at(21), at(23))),
            false,
        );
        assert!(!t.is_quiet());
        let r = t.render_with_notes(&[]);
        assert!(r.contains("◆ GOLDEN"), "golden still marked: {r}");
        assert!(
            r.contains("⌀ spread-hour"),
            "rubbish-candle note rendered: {r}"
        );
        // Bare (not-confirmed) suppression keeps the "rubbish candle" wording, and
        // now shows the block window bounds.
        assert!(r.contains("rubbish candle"), "not-confirmed wording: {r}");
        assert!(r.contains("UTC"), "block window bounds shown: {r}");
    }

    /// A golden that ALSO confirmed on a spread-hour bar renders the enriched
    /// line — "signal confirmed, not entering (spread-hour suppressed)" plus the
    /// block window — instead of the bare rubbish-candle note. This is the case
    /// the operator asked for: a ready-to-enter setup held back only by the
    /// spread hour.
    #[test]
    fn spread_hour_confirmed_signal_renders_not_entering_with_window() {
        let s = state(Phase::AwaitEntry);
        // No fresh mark on this bar — the golden printed a couple of bars earlier
        // and only CONFIRMED here. The line must still render (not gated on a
        // fresh mark) and the bar must not be quiet.
        let t = BarTrace::diff(
            at(23),
            &s,
            &s,
            Vec::new(),
            None,
            Vec::new(),
            None,
            true,
            Some((at(21), at(23))),
            true,
        );
        assert!(
            !t.is_quiet(),
            "a confirmation-while-suppressed bar is noteworthy"
        );
        let r = t.render_with_notes(&[]);
        assert!(
            r.contains("signal confirmed, not entering"),
            "confirmed-but-suppressed wording: {r}"
        );
        assert!(r.contains("spread-hour suppressed"), "reason shown: {r}");
        assert!(r.contains("UTC"), "block window bounds shown: {r}");
        assert!(
            !r.contains("rubbish candle"),
            "confirmed case drops the bare rubbish wording: {r}"
        );
    }

    /// A spread-hour bar with NO detected mark stays quiet — we don't want every
    /// spread hour adding a line to the trace.
    #[test]
    fn spread_hour_without_a_mark_stays_quiet() {
        let s = state(Phase::AwaitEntry);
        let t = BarTrace::diff(
            at(21),
            &s,
            &s,
            Vec::new(),
            None,
            Vec::new(),
            None,
            true,
            Some((at(21), at(23))),
            false,
        );
        assert!(t.is_quiet(), "spread hour alone is not noteworthy");
        assert_eq!(t.render_with_notes(&[]), "");
    }
}
