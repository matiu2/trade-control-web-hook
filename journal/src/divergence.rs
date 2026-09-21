//! Replay-vs-live divergence diff — the bug-hunting heart of the tool.
//!
//! The project's core invariant is **replay == worker**: the offline
//! `replay-candles` simulation must fire the same rules, in the same order, that
//! the live worker actually did. This module extracts a comparable set of "fire
//! facts" from each side and classifies where they disagree.
//!
//! Both sides ultimately reference rule fires keyed by `rule_id`:
//!
//! * the **live** side is the `plan timeline` JSON — the engine fires are the
//!   `ticks[].eval.fired[]` objects (each carries a `rule_id` + `intent.action`),
//!   parsed the same way `timeline::parse_events` reads them;
//! * the **replay** side is the plain-text `replay-candles` report — each fire
//!   is a line `<ts>  <LABEL> (<rule_id>) — …`, where the `rule_id` sits in the
//!   first parenthesised group.
//!
//! We normalise both timestamps to `YYYY-MM-DD HH:MM` Brisbane (the live side is
//! already Brisbane via `ts_to_bne`; the replay side prints `… +10:00`, so we
//! drop the seconds + offset) so a *timing* divergence — the same rule firing on
//! a different bar — is comparable.

use chrono::DateTime;
use serde_json::Value;

use crate::timeline::ts_to_bne;

/// One rule fire, reduced to the fields the diff joins on. `rule_id` is the join
/// key; `action` is informational (`pause`, `enter`, …); `ts` is normalised
/// Brisbane `YYYY-MM-DD HH:MM` so the two sides' timings line up for comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FireFact {
    pub rule_id: String,
    pub action: Option<String>,
    pub ts: String,
    /// How long after its bar CLOSED the cron observed this fire, in seconds —
    /// `tick_ts - (candle.time + granularity)`. `None` on the replay side (a
    /// report has no wall clock) and for a live fire whose bundle carries no
    /// plan granularity, where the bar length is unknown.
    ///
    /// Measured across the 20 live staging plans: price/pattern rules land
    /// 0-42s (median 12s, a 5s cron), while time-scheduled news rules sit
    /// 1802-12634s because a news window opens mid-bar. **Negative** means the
    /// cron evaluated the bar BEFORE it closed, so the close it read was
    /// provisional — see [`FORMING_BAR`] and [`diff`].
    pub lateness_secs: Option<i64>,
}

/// Outcome-level facts parsed from the replay summary line, for a coarse
/// sanity-check alongside the per-fire diff. All optional — a non-`--simulate`
/// report has no TP/SL/Net-R.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplayOutcome {
    pub done: Option<bool>,
    pub final_phase: Option<String>,
    pub fires: Option<usize>,
    pub tp: Option<usize>,
    pub sl: Option<usize>,
    pub net_r: Option<String>,
}

/// One timing divergence: the same rule, on bars that do not line up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingDelta {
    pub rule_id: String,
    pub live_ts: String,
    pub replay_ts: String,
    /// The live fire's lateness past its bar close; see [`FireFact`].
    pub lateness_secs: Option<i64>,
}

/// The classified diff between the live fires and the replay fires.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Divergences {
    /// Rule ids that fired on both sides, with matching normalised timing.
    pub matches: Vec<FireFact>,
    /// Fired live but not in the replay (replay under-fired).
    pub live_only: Vec<FireFact>,
    /// Fired in the replay but not live (replay over-fired).
    pub replay_only: Vec<FireFact>,
    /// Same rule id fired on both sides but not comparably: a different bar,
    /// or the same bar read before it closed. `lateness_secs` is the live
    /// side's, so the view can show WHEN the cron observed the bar alongside
    /// which bar it was.
    pub timing: Vec<TimingDelta>,
}

impl Divergences {
    /// Everything lines up — same rule ids, same bars, nothing one-sided.
    pub fn is_clean(&self) -> bool {
        self.live_only.is_empty() && self.replay_only.is_empty() && self.timing.is_empty()
    }
}

/// Normalise a replay-report Brisbane timestamp (`2026-07-23 13:00:00 +10:00`)
/// to the `YYYY-MM-DD HH:MM` form the live side uses, by keeping the first two
/// whitespace-separated tokens and trimming the seconds off the time. Tolerant:
/// an unexpected shape is returned trimmed rather than dropped.
fn normalize_replay_ts(raw: &str) -> String {
    let mut parts = raw.split_whitespace();
    let (Some(date), Some(time)) = (parts.next(), parts.next()) else {
        return raw.trim().to_string();
    };
    // `13:00:00` → `13:00`; a bare `13:00` is left as-is.
    let hhmm = match (time.find(':'), time.rfind(':')) {
        (Some(_), Some(last)) if last > time.find(':').unwrap_or(0) => &time[..last],
        _ => time,
    };
    format!("{date} {hhmm}")
}

/// Parse the fire facts out of a plain-text `replay-candles` report. Scans each
/// line for the fire shape `<ts>  <LABEL> (<rule_id>) — …`, pulling the leading
/// Brisbane timestamp and the `rule_id` from the first parenthesised group.
/// Lines that don't match (the header, detector/sentiment lines, the summary,
/// blank lines, or stray tracing noise if stdout+stderr were merged) are
/// skipped. The action is inferred from the uppercase label prefix.
pub fn parse_replay_fires(report: &str) -> Vec<FireFact> {
    report.lines().filter_map(parse_replay_fire_line).collect()
}

/// Parse one report line into a [`FireFact`], or `None` if it isn't a fire line.
fn parse_replay_fire_line(line: &str) -> Option<FireFact> {
    // A fire line starts with a Brisbane timestamp `YYYY-MM-DD HH:MM:SS +10:00`.
    // Cheap gate: it must contain `+10:00` and a `(rule_id)` group, and must not
    // be the summary line.
    if line.starts_with("Done:") || line.starts_with("Plan ") {
        return None;
    }
    let rule_id = first_parenthesised(line)?;
    // Skip anything whose parenthesised token clearly isn't a rule id (e.g. the
    // sentiment "(no released events …)" line): rule ids have no spaces.
    if rule_id.contains(' ') {
        return None;
    }
    // The timestamp is everything up to the double-space before the label.
    let ts_raw = line.split("  ").next().unwrap_or("").trim();
    if ts_raw.is_empty() || !ts_raw.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let ts = normalize_replay_ts(ts_raw);
    let action = replay_action_from_label(line);
    Some(FireFact {
        rule_id,
        action,
        ts,
        // A replay report has no wall clock — it never observed anything late.
        lateness_secs: None,
    })
}

/// The contents of the first `(...)` group in a line, if any.
fn first_parenthesised(line: &str) -> Option<String> {
    let open = line.find('(')?;
    let rest = &line[open + 1..];
    let close = rest.find(')')?;
    Some(rest[..close].to_string())
}

/// Infer the fire's action from the report's uppercase label prefix. This maps
/// the operator-facing wording (`PAUSE entries`, `NEWS START`, `entry #1
/// placed`, `close-on-reversal`, …) back to the wire action name so it lines up
/// with the live side's `intent.action`. Best-effort — an unrecognised label
/// yields `None`, which never blocks the rule-id join.
fn replay_action_from_label(line: &str) -> Option<String> {
    let after_ts = line.split("  ").nth(1).unwrap_or(line).trim_start();
    let label = after_ts;
    let action = if label.starts_with("PAUSE") {
        "pause"
    } else if label.starts_with("RESUME") {
        "resume"
    } else if label.starts_with("NEWS START") {
        "news-start"
    } else if label.starts_with("NEWS END") {
        "news-end"
    } else if label.starts_with("prep") {
        "prep"
    } else if label.starts_with("close") {
        "close"
    } else if label.contains("placed") || label.contains("FILLED") || label.starts_with("entry") {
        "enter"
    } else {
        return None;
    };
    Some(action.to_string())
}

/// Parse the replay report's trailing summary line into a [`ReplayOutcome`].
/// The line looks like `Done: false  |  final phase: AwaitBreakAndClose  |
/// fires: 4  |  TP: 0  SL: 0  |  Net R: +0.00  |  …` — the `TP`/`SL`/`Net R`
/// segments are only present under `--simulate`, so they parse to `None` when
/// absent. Tolerant: a missing summary line yields an all-`None` outcome.
pub fn parse_replay_outcome(report: &str) -> ReplayOutcome {
    let Some(line) = report
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("Done:"))
    else {
        return ReplayOutcome::default();
    };
    let mut out = ReplayOutcome::default();
    for seg in line.split('|') {
        let seg = seg.trim();
        if let Some(v) = seg.strip_prefix("Done:") {
            out.done = match v.trim() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
        } else if let Some(v) = seg.strip_prefix("final phase:") {
            out.final_phase = Some(v.trim().to_string());
        } else if let Some(v) = seg.strip_prefix("fires:") {
            out.fires = v.trim().parse().ok();
        } else if let Some(v) = seg.strip_prefix("Net R:") {
            out.net_r = Some(v.trim().to_string());
        } else if seg.starts_with("TP:") {
            // `TP: 0  SL: 0` share a segment (no `|` between them).
            out.tp = seg.split_whitespace().nth(1).and_then(|s| s.parse().ok());
            out.sl = seg.split_whitespace().nth(3).and_then(|s| s.parse().ok());
        }
    }
    out
}

/// Extract the live engine fires from the `plan timeline` JSON. Reads only the
/// `ticks[].eval.fired[]` objects (never the inbound `records`, which include
/// this tool's own recursive plan-show/plan-timeline queries), keyed by
/// `rule_id`, timestamped by the **firing bar** (`fired[].candle.time`)
/// normalised to Brisbane `YYYY-MM-DD HH:MM` (the same `ts_to_bne` the
/// timeline view uses).
///
/// **Not the tick.** `tick_ts` is the cron's wall clock — when the scheduler
/// happened to observe the bar, not when the rule fired. A cron that runs at
/// half past stamps a 10:00 bar `10:30`, and the replay (which prints the bar)
/// then looks half an hour out. Both sides run the same `evaluate_plan`, and
/// its `FiredIntent` carries the firing `candle` on each side, so the bar is
/// the one instant they can agree on. `candle.time` is the bar's **open** (the
/// OANDA / TradeNation convention), matching what the replay report prints.
///
/// **This is a display fix, and a narrow one.** It only moves a stamp where
/// `tick_ts` and `candle.time` disagree — a cron that ticked mid-bar on a
/// granularity coarser than its own period. It does NOT explain a one-bar Δ
/// where the two agree: there the engine really did fire on a different bar
/// from the replay, and the cause is upstream. The known instance is the
/// TradeNation forming-bar bug (`BUG-tn-native-granularity-emits-forming-bar.md`):
/// the adapter served the still-open bar, so the worker fired an `on_close`
/// rule against a provisional close. Timelines recorded before that fix keep
/// their forming-bar stamps, so their Δ is a TRUE record of what live did —
/// do not try to normalise it away here.
pub fn live_fires(timeline_json: &str) -> Vec<FireFact> {
    let Ok(v) = serde_json::from_str::<Value>(timeline_json) else {
        return Vec::new();
    };
    let Some(ticks) = v.get("ticks").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    ticks.iter().flat_map(live_fires_from_tick).collect()
}

/// The fire facts from a single tick object.
fn live_fires_from_tick(tick: &Value) -> Vec<FireFact> {
    let tick_raw = tick.get("tick_ts").and_then(|x| x.as_str()).unwrap_or("");
    let tick_ts = ts_to_bne(tick_raw);
    let bar_secs = tick
        .get("plan")
        .and_then(|p| p.get("granularity"))
        .and_then(|g| g.as_str())
        .and_then(granularity_secs);
    let Some(fired) = tick
        .get("eval")
        .and_then(|e| e.get("fired"))
        .and_then(|f| f.as_array())
    else {
        return Vec::new();
    };
    fired
        .iter()
        .filter_map(|rule| {
            let rule_id = rule
                .get("rule_id")
                .and_then(|x| x.as_str())
                .or_else(|| rule.as_str())?;
            let action = rule
                .get("intent")
                .and_then(|i| i.get("action"))
                .and_then(|a| a.as_str())
                .map(str::to_string);
            Some(FireFact {
                rule_id: rule_id.to_string(),
                action,
                ts: fire_bar(rule).unwrap_or_else(|| tick_ts.clone()),
                lateness_secs: lateness_secs(rule, tick_raw, bar_secs),
            })
        })
        .collect()
}

/// The Brisbane `YYYY-MM-DD HH:MM` of the bar a fire triggered on, read from
/// the `FiredIntent`'s own `candle.time`. `None` when the fire carries no
/// candle (an older bundle schema), leaving the caller to fall back to the
/// tick rather than dropping the fire from the diff.
fn fire_bar(rule: &Value) -> Option<String> {
    let t = rule.get("candle")?.get("time")?.as_str()?;
    Some(ts_to_bne(t))
}

/// The bar length of a plan `granularity` as the timeline spells it (lowercase,
/// e.g. `h1`), in seconds. `None` for anything unrecognised, which leaves the
/// lateness unknown rather than computing it against a guessed bar.
fn granularity_secs(g: &str) -> Option<i64> {
    match g.to_ascii_lowercase().as_str() {
        "m1" => Some(60),
        "m5" => Some(5 * 60),
        "m15" => Some(15 * 60),
        "m30" => Some(30 * 60),
        "h1" => Some(60 * 60),
        "h4" => Some(4 * 60 * 60),
        "d1" | "d" => Some(24 * 60 * 60),
        "w" => Some(7 * 24 * 60 * 60),
        _ => None,
    }
}

/// Seconds between a fire's bar CLOSING and the cron tick that observed it.
/// Negative when the tick landed inside the still-forming bar. `None` when
/// either instant, or the bar length, is missing.
fn lateness_secs(rule: &Value, tick_raw: &str, bar_secs: Option<i64>) -> Option<i64> {
    let bar = bar_secs?;
    let open = rule.get("candle")?.get("time")?.as_str()?;
    let open = DateTime::parse_from_rfc3339(open).ok()?;
    let tick = DateTime::parse_from_rfc3339(tick_raw).ok()?;
    Some((tick - open).num_seconds() - bar)
}

/// A fire whose lateness is below this evaluated a bar that had not closed yet.
///
/// The boundary is the bar close itself, not a tolerance window, because the
/// live data made a magnitude threshold unnecessary. Across 96 recorded fires
/// on the 20 staging plans, the price/pattern rules this diff judges land
/// **0-42s** past their bar close (median 12s, on a 5s engine tick), and the
/// only larger values — 1802-12634s — are time-scheduled `pause`/`resume`/
/// `news-*` rules firing mid-bar by design, which are NOT divergences. So no
/// positive lateness distinguishes a healthy fire from a faulty one; only the
/// SIGN does. A fire at exactly `0` read a complete bar and is fine.
const FORMING_BAR: i64 = 0;

/// Whether a same-bar match should still be reported as a timing divergence.
///
/// One case survives the same-bar check: **negative lateness**. The cron read
/// the bar mid-formation, so the close it fired on was provisional and may
/// never have been the bar's real close. That is a genuine disagreement about
/// what the engine saw, not jitter. The known cause is the TradeNation
/// forming-bar bug (`BUG-tn-native-granularity-emits-forming-bar.md`);
/// timelines recorded before that fix keep the signature, and must keep
/// reporting it.
///
/// **Unknown lateness is NOT a divergence.** An older bundle carries no plan
/// granularity, so the bar length — and therefore the close — is unknowable.
/// The bars themselves still agree, and that is positive evidence; inventing a
/// divergence from a missing field would flag every pre-schema timeline and
/// re-create the noise this check exists to remove. Absence of evidence is
/// not evidence of divergence.
///
/// Large POSITIVE lateness is deliberately NOT a divergence. Every one of the
/// 12 such fires measured is a time-scheduled `pause`/`resume`/`news-*` rule
/// firing mid-bar by design; flagging them would re-create the noise this
/// tolerance exists to remove.
fn same_bar_is_still_divergent(lateness: Option<i64>) -> bool {
    matches!(lateness, Some(secs) if secs < FORMING_BAR)
}

/// Classify the live vs replay fire sets by `rule_id`. A rule id present on both
/// sides is a **match** (with a **timing** divergence noted if its normalised ts
/// differs); present on only one side is **live-only** (replay under-fired) or
/// **replay-only** (replay over-fired). Duplicate rule ids on a side (a
/// multi-shot enter re-firing) are matched positionally by first occurrence.
pub fn diff(live: &[FireFact], replay: &[FireFact]) -> Divergences {
    let mut out = Divergences::default();
    let mut replay_used = vec![false; replay.len()];

    for lf in live {
        match replay
            .iter()
            .enumerate()
            .find(|(i, rf)| !replay_used[*i] && rf.rule_id == lf.rule_id)
        {
            Some((i, rf)) => {
                replay_used[i] = true;
                out.matches.push(lf.clone());
                let different_bar = rf.ts != lf.ts;
                if different_bar || same_bar_is_still_divergent(lf.lateness_secs) {
                    out.timing.push(TimingDelta {
                        rule_id: lf.rule_id.clone(),
                        live_ts: lf.ts.clone(),
                        replay_ts: rf.ts.clone(),
                        lateness_secs: lf.lateness_secs,
                    });
                }
            }
            None => out.live_only.push(lf.clone()),
        }
    }
    for (i, rf) in replay.iter().enumerate() {
        if !replay_used[i] {
            out.replay_only.push(rf.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLAY: &str = include_str!("../tests/fixtures/replay_report.txt");
    const TIMELINE: &str = include_str!("../tests/fixtures/plan_timeline.json");
    /// A single `01-veto-too-high` fire on the 11:00 Brisbane bar, evaluated by
    /// a cron tick that ran at 12:11 Brisbane — the late-cron shape that made
    /// the journal report a phantom one-hour timing divergence.
    const LATE_CRON: &str = include_str!("../tests/fixtures/veto_late_cron_timeline.json");

    #[test]
    fn parses_the_four_replay_fires_with_rule_ids() {
        let fires = parse_replay_fires(REPLAY);
        assert_eq!(fires.len(), 4, "AUD_CAD replay fires pause/resume/news×2");
        // Every fire carries a real rule id (not the header/summary/sentiment).
        assert!(
            fires
                .iter()
                .all(|f| f.rule_id.starts_with(|c: char| c.is_ascii_digit()))
        );
        // All four fired on the same Brisbane bar in the replay.
        assert!(
            fires.iter().all(|f| f.ts == "2026-07-23 13:00"),
            "{fires:?}"
        );
        // Actions are inferred from the labels.
        let actions: Vec<_> = fires.iter().filter_map(|f| f.action.clone()).collect();
        assert!(actions.contains(&"pause".to_string()), "{actions:?}");
        assert!(actions.contains(&"news-start".to_string()), "{actions:?}");
    }

    #[test]
    fn skips_non_fire_report_lines() {
        // The sentiment "(no released events …)" line has a parenthesised group
        // but is not a fire — it must be skipped (its group has spaces + no ts).
        let fires = parse_replay_fires(REPLAY);
        assert!(fires.iter().all(|f| !f.rule_id.contains("released")));
    }

    #[test]
    fn parses_the_replay_outcome_summary() {
        // The real `replay-candles` report always carries the TP/SL/Net-R
        // segments (the report renders them regardless of the journal's flags).
        let o = parse_replay_outcome(REPLAY);
        assert_eq!(o.done, Some(false));
        assert_eq!(o.final_phase.as_deref(), Some("AwaitBreakAndClose"));
        assert_eq!(o.fires, Some(4));
        assert_eq!(o.tp, Some(0));
        assert_eq!(o.sl, Some(0));
        assert_eq!(o.net_r.as_deref(), Some("+0.00"));
    }

    #[test]
    fn parses_a_winning_outcome_with_tp_sl_and_net_r() {
        let line = "Done: true  |  final phase: Done  |  fires: 2  |  TP: 1  SL: 1  |  Net R: +0.50  |  $100k acct (1%/trade): $100500 (+500)";
        let o = parse_replay_outcome(line);
        assert_eq!(o.done, Some(true));
        assert_eq!(o.fires, Some(2));
        assert_eq!(o.tp, Some(1));
        assert_eq!(o.sl, Some(1));
        assert_eq!(o.net_r.as_deref(), Some("+0.50"));
    }

    #[test]
    fn outcome_defaults_all_none_without_a_summary_line() {
        let o = parse_replay_outcome("Plan foo (X, H1) — 0 fire(s)\nno summary here\n");
        assert_eq!(o, ReplayOutcome::default());
    }

    #[test]
    fn live_fires_reads_ticks_not_records() {
        let fires = live_fires(TIMELINE);
        // The fixture has 4 engine fires; the inbound records (register /
        // plan-show / plan-timeline — this tool's own noise) are excluded.
        assert_eq!(fires.len(), 4, "{fires:?}");
        let ids: Vec<_> = fires.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(ids.contains(&"01-pause-1784741400-1784770200"));
        assert!(ids.contains(&"02-news-end-1784770200-1784773800"));
    }

    #[test]
    fn aud_cad_diff_is_four_matches_and_four_timing_divergences() {
        // The headline test: live fires pause/resume/news-start/news-end on the
        // 02:00/10:00/10:00/11:00 Brisbane bars; the replay fires all four at
        // 13:00. So the rule ids all match, but every one is a GENUINE timing
        // divergence — a real disagreement about the bar, not the late-cron
        // artefact `a_late_cron_tick_is_not_a_timing_divergence` covers. (The
        // live bars read off each fire's own candle; this fixture's cron ticks
        // ran at 03:30/11:30/12:30, which is what used to be reported here.)
        let live = live_fires(TIMELINE);
        let replay = parse_replay_fires(REPLAY);
        let d = diff(&live, &replay);
        assert_eq!(d.matches.len(), 4, "all four rule ids fire on both sides");
        assert!(d.live_only.is_empty(), "no under-fire: {:?}", d.live_only);
        assert!(
            d.replay_only.is_empty(),
            "no over-fire: {:?}",
            d.replay_only
        );
        assert_eq!(d.timing.len(), 4, "every fire is on a different bar");
        assert!(!d.is_clean(), "a timing divergence is not clean");
        // Spot-check one timing tuple: pause fired on the live 02:00 bar
        // (observed by the 03:30 cron tick), replay 13:00.
        let pause = d
            .timing
            .iter()
            .find(|d| d.rule_id.starts_with("01-pause"))
            .expect("pause timing divergence");
        assert_eq!(pause.live_ts, "2026-07-23 02:00", "live pause bar");
        assert_eq!(pause.replay_ts, "2026-07-23 13:00", "replay pause bar");
    }

    #[test]
    fn a_live_fire_is_stamped_with_its_bar_not_the_cron_tick() {
        // The cron evaluated the 11:00 bar at 12:11; the fire belongs to the
        // bar, which is the only instant the replay can also name.
        let fires = live_fires(LATE_CRON);
        assert_eq!(fires.len(), 1, "{fires:?}");
        assert_eq!(
            fires[0].ts, "2026-09-07 11:00",
            "the firing bar, not the 12:11 cron tick that observed it"
        );
    }

    #[test]
    fn a_late_cron_tick_is_not_a_timing_divergence() {
        // The regression this fixes: both sides agree the veto fired on the
        // 11:00 bar, so the journal must report no divergence at all.
        let live = live_fires(LATE_CRON);
        let replay = parse_replay_fires(
            "2026-09-07 11:00:00 +10:00  PAUSE entries (01-veto-too-high) — entry level exceeded  (close=1.22641)\n",
        );
        assert_eq!(replay.len(), 1, "{replay:?}");
        let d = diff(&live, &replay);
        assert_eq!(d.matches.len(), 1);
        assert!(
            d.timing.is_empty(),
            "same bar, different observation clock: {:?}",
            d.timing
        );
        assert!(d.is_clean());
    }

    #[test]
    fn a_fire_on_a_genuinely_different_bar_is_still_a_divergence() {
        // The guard on the fix: stamping by bar must not blind the diff to a
        // real disagreement. Same rule, replay one bar later — still reported.
        let live = live_fires(LATE_CRON);
        let replay = parse_replay_fires(
            "2026-09-07 12:00:00 +10:00  PAUSE entries (01-veto-too-high) — entry level exceeded  (close=1.22641)\n",
        );
        let d = diff(&live, &replay);
        assert_eq!(d.timing.len(), 1, "a real one-bar divergence survives");
        assert_eq!(d.timing[0].live_ts, "2026-09-07 11:00", "live bar");
        assert_eq!(d.timing[0].replay_ts, "2026-09-07 12:00", "replay bar");
    }

    #[test]
    fn a_fire_without_a_candle_falls_back_to_the_tick() {
        // Tolerance: a bundle whose fire carries no candle (an older schema)
        // must still produce a fact rather than vanish from the diff.
        let json = r#"{"ticks":[{"tick_ts":"2026-09-07T02:11:43Z",
          "eval":{"fired":[{"rule_id":"01-veto-too-high"}]}}]}"#;
        let fires = live_fires(json);
        assert_eq!(fires.len(), 1, "{fires:?}");
        assert_eq!(fires[0].ts, "2026-09-07 12:11", "falls back to the tick");
    }

    #[test]
    fn a_fire_carries_how_late_the_cron_observed_its_bar() {
        // 11:00Z bar on h1 closes 12:00Z; the cron ticked 12:00:14Z.
        let json = r#"{"ticks":[{"tick_ts":"2026-09-07T12:00:14Z",
          "plan":{"granularity":"h1"},
          "eval":{"fired":[{"rule_id":"01-veto-too-high",
            "candle":{"time":"2026-09-07T11:00:00Z"}}]}}]}"#;
        let fires = live_fires(json);
        assert_eq!(fires[0].lateness_secs, Some(14));
    }

    #[test]
    fn a_fire_on_a_bar_that_had_not_closed_yet_is_negative() {
        // THE forming-bar signature: the cron ticked 21s INTO the 02:00Z bar,
        // which does not close until 03:00Z. Recorded on AUD/NZD before the
        // TradeNation adapter was fixed.
        let json = r#"{"ticks":[{"tick_ts":"2026-09-07T02:00:21Z",
          "plan":{"granularity":"h1"},
          "eval":{"fired":[{"rule_id":"01-veto-too-high",
            "candle":{"time":"2026-09-07T02:00:00Z"}}]}}]}"#;
        let fires = live_fires(json);
        assert_eq!(fires[0].lateness_secs, Some(-3579));
    }

    #[test]
    fn lateness_is_unknown_without_a_granularity() {
        // An older bundle carries no plan granularity: report None rather than
        // guessing a bar length and inventing a verdict from it.
        let json = r#"{"ticks":[{"tick_ts":"2026-09-07T12:00:14Z",
          "eval":{"fired":[{"rule_id":"01-veto-too-high",
            "candle":{"time":"2026-09-07T11:00:00Z"}}]}}]}"#;
        assert_eq!(live_fires(json)[0].lateness_secs, None);
    }

    #[test]
    fn ordinary_cron_jitter_on_the_same_bar_is_not_a_divergence() {
        // The headline: same bar, cron 14s late — the measured median across
        // the 20 live staging plans. Must not be reported.
        let live = live_fires(
            r#"{"ticks":[{"tick_ts":"2026-09-07T12:00:14Z",
              "plan":{"granularity":"h1"},
              "eval":{"fired":[{"rule_id":"01-veto-too-high",
                "candle":{"time":"2026-09-07T11:00:00Z"}}]}}]}"#,
        );
        let replay = parse_replay_fires(
            "2026-09-07 21:00:00 +10:00  Veto (01-veto-too-high) — x  (close=1.2)\n",
        );
        let d = diff(&live, &replay);
        assert_eq!(d.matches.len(), 1);
        assert!(d.timing.is_empty(), "14s of jitter is not a divergence");
        assert!(d.is_clean());
    }

    #[test]
    fn a_fire_before_its_bar_closed_is_always_a_divergence() {
        // Even though both sides name the SAME bar, live evaluated it while it
        // was still forming, so its close was provisional. That is a real
        // disagreement about what the engine saw, not cron jitter.
        let live = live_fires(
            r#"{"ticks":[{"tick_ts":"2026-09-07T02:00:21Z",
              "plan":{"granularity":"h1"},
              "eval":{"fired":[{"rule_id":"01-veto-too-high",
                "candle":{"time":"2026-09-07T02:00:00Z"}}]}}]}"#,
        );
        let replay = parse_replay_fires(
            "2026-09-07 12:00:00 +10:00  Veto (01-veto-too-high) — x  (close=1.2)\n",
        );
        let d = diff(&live, &replay);
        assert_eq!(d.timing.len(), 1, "a forming bar is always reported");
        assert!(!d.is_clean());
    }

    #[test]
    fn a_tick_landing_exactly_on_the_bar_close_read_a_complete_bar() {
        // The boundary: lateness 0 means the cron fired the instant the bar
        // closed, so it read a COMPLETE bar — not a forming one. Three fires
        // in the live staging sample sit exactly here, so this is reachable,
        // and `<=` would wrongly call every one of them a divergence.
        let live = live_fires(
            r#"{"ticks":[{"tick_ts":"2026-09-07T12:00:00Z",
              "plan":{"granularity":"h1"},
              "eval":{"fired":[{"rule_id":"01-veto-too-high",
                "candle":{"time":"2026-09-07T11:00:00Z"}}]}}]}"#,
        );
        assert_eq!(live[0].lateness_secs, Some(0));
        let replay = parse_replay_fires(
            "2026-09-07 21:00:00 +10:00  Veto (01-veto-too-high) — x  (close=1.2)\n",
        );
        let d = diff(&live, &replay);
        assert!(d.timing.is_empty(), "0s is a closed bar, not a forming one");
        assert!(d.is_clean());
    }

    #[test]
    fn a_news_rule_firing_deep_inside_its_bar_is_not_a_divergence() {
        // Measured: all 12 time-scheduled fires (pause/resume/news) sit
        // 1802-12634s past their H4 bar close, because a news window opens
        // mid-bar. Thresholding lateness alone would flag every one of them.
        let live = live_fires(
            r#"{"ticks":[{"tick_ts":"2026-09-10T12:30:33Z",
              "plan":{"granularity":"h4"},
              "eval":{"fired":[{"rule_id":"01-news-start-1789043400-1789047000",
                "candle":{"time":"2026-09-10T05:00:00Z"}}]}}]}"#,
        );
        assert_eq!(live[0].lateness_secs, Some(12633));
        let replay = parse_replay_fires(
            "2026-09-10 15:00:00 +10:00  NEWS START (01-news-start-1789043400-1789047000) — x\n",
        );
        let d = diff(&live, &replay);
        assert!(
            d.timing.is_empty(),
            "same bar, news fires mid-bar by design: {:?}",
            d.timing
        );
    }

    #[test]
    fn a_genuinely_different_bar_is_still_a_divergence() {
        // The guard: none of the above may blind the diff to a real one.
        let live = live_fires(
            r#"{"ticks":[{"tick_ts":"2026-09-07T12:00:05Z",
              "plan":{"granularity":"h1"},
              "eval":{"fired":[{"rule_id":"01-veto-too-high",
                "candle":{"time":"2026-09-07T11:00:00Z"}}]}}]}"#,
        );
        let replay = parse_replay_fires(
            "2026-09-07 22:00:00 +10:00  Veto (01-veto-too-high) — x  (close=1.2)\n",
        );
        let d = diff(&live, &replay);
        assert_eq!(d.timing.len(), 1);
        assert_eq!(d.timing[0].live_ts, "2026-09-07 21:00", "live bar");
        assert_eq!(d.timing[0].replay_ts, "2026-09-07 22:00", "replay bar");
    }

    #[test]
    fn live_only_and_replay_only_are_detected() {
        let live = vec![
            FireFact {
                rule_id: "05-enter".into(),
                action: Some("enter".into()),
                ts: "2026-07-23 08:00".into(),
                lateness_secs: None,
            },
            FireFact {
                rule_id: "01-only-live".into(),
                action: Some("pause".into()),
                ts: "2026-07-23 09:00".into(),
                lateness_secs: None,
            },
        ];
        let replay = vec![
            FireFact {
                rule_id: "05-enter".into(),
                action: Some("enter".into()),
                ts: "2026-07-23 08:00".into(),
                lateness_secs: None,
            },
            FireFact {
                rule_id: "02-only-replay".into(),
                action: Some("close".into()),
                ts: "2026-07-23 10:00".into(),
                lateness_secs: None,
            },
        ];
        let d = diff(&live, &replay);
        assert_eq!(d.matches.len(), 1);
        assert_eq!(d.live_only.len(), 1);
        assert_eq!(d.live_only[0].rule_id, "01-only-live");
        assert_eq!(d.replay_only.len(), 1);
        assert_eq!(d.replay_only[0].rule_id, "02-only-replay");
        assert!(
            d.timing.is_empty(),
            "the matched enter fired on the same bar"
        );
    }

    #[test]
    fn clean_diff_when_everything_agrees() {
        let fires = vec![FireFact {
            rule_id: "05-enter".into(),
            action: Some("enter".into()),
            ts: "2026-07-23 08:00".into(),
            lateness_secs: None,
        }];
        let d = diff(&fires, &fires);
        assert!(d.is_clean());
        assert_eq!(d.matches.len(), 1);
    }
}
