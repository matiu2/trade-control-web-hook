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

/// The classified diff between the live fires and the replay fires.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Divergences {
    /// Rule ids that fired on both sides, with matching normalised timing.
    pub matches: Vec<FireFact>,
    /// Fired live but not in the replay (replay under-fired).
    pub live_only: Vec<FireFact>,
    /// Fired in the replay but not live (replay over-fired).
    pub replay_only: Vec<FireFact>,
    /// Same rule id fired on both sides but on a different bar:
    /// `(rule_id, live_ts, replay_ts)`.
    pub timing: Vec<(String, String, String)>,
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
/// `rule_id`, timestamped by the tick's `tick_ts` normalised to Brisbane
/// `YYYY-MM-DD HH:MM` (the same `ts_to_bne` the timeline view uses).
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
    let ts = tick.get("tick_ts").and_then(|x| x.as_str()).unwrap_or("");
    let ts = ts_to_bne(ts);
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
                ts: ts.clone(),
            })
        })
        .collect()
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
                if rf.ts != lf.ts {
                    out.timing
                        .push((lf.rule_id.clone(), lf.ts.clone(), rf.ts.clone()));
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
        // The headline test: live fires pause/resume/news-start/news-end spread
        // across 03:30–12:30 Brisbane; the replay fires all four at 13:00. So the
        // rule ids all match, but every one is a timing divergence.
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
        // Spot-check one timing tuple: pause fired live 03:30, replay 13:00.
        let pause = d
            .timing
            .iter()
            .find(|(id, _, _)| id.starts_with("01-pause"))
            .expect("pause timing divergence");
        assert_eq!(pause.1, "2026-07-23 03:30", "live pause bar");
        assert_eq!(pause.2, "2026-07-23 13:00", "replay pause bar");
    }

    #[test]
    fn live_only_and_replay_only_are_detected() {
        let live = vec![
            FireFact {
                rule_id: "05-enter".into(),
                action: Some("enter".into()),
                ts: "2026-07-23 08:00".into(),
            },
            FireFact {
                rule_id: "01-only-live".into(),
                action: Some("pause".into()),
                ts: "2026-07-23 09:00".into(),
            },
        ];
        let replay = vec![
            FireFact {
                rule_id: "05-enter".into(),
                action: Some("enter".into()),
                ts: "2026-07-23 08:00".into(),
            },
            FireFact {
                rule_id: "02-only-replay".into(),
                action: Some("close".into()),
                ts: "2026-07-23 10:00".into(),
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
        }];
        let d = diff(&fires, &fires);
        assert!(d.is_clean());
        assert_eq!(d.matches.len(), 1);
    }
}
