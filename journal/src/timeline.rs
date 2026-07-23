//! Timeline parsing: turn `plan timeline --json` (`{records, ticks}`) into
//! display event lines, plus the two info-bar derivations (entry timestamp,
//! final outcome).
//!
//! We parse loosely over `serde_json::Value` and pull only the fields we show —
//! the full `PlanTimeline`/`TickBundle` types live in `trade_control_core` but
//! depending on that crate would drag the whole worker tree into this tool.

use chrono::{DateTime, FixedOffset, Utc};
use serde_json::Value;

/// One line in the rendered timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Brisbane-formatted timestamp.
    pub ts: String,
    /// `⊙` inbound signed alert, `•` engine fire.
    pub marker: char,
    pub text: String,
}

/// Brisbane (UTC+10, no DST) — the zone every trade-control tool renders in.
fn bne(dt: DateTime<Utc>) -> String {
    let brisbane = FixedOffset::east_opt(10 * 3600).expect("10h is valid");
    dt.with_timezone(&brisbane)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Parse an RFC3339 timestamp to Brisbane, or echo the raw string on failure.
fn ts_to_bne(raw: &str) -> String {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| bne(dt.with_timezone(&Utc)))
        .unwrap_or_else(|_| raw.to_string())
}

/// Extract an ordered list of events from the timeline JSON. Inbound `records`
/// become `⊙` lines by `ts`; engine `ticks` that fired become `•` lines.
pub fn parse_events(json: &str) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let mut events = Vec::new();

    // Inbound signed-alert records: show the action + outcome.
    if let Some(records) = v.get("records").and_then(|r| r.as_array()) {
        for rec in records {
            let ts = rec.get("ts").and_then(|x| x.as_str()).unwrap_or("");
            let action = record_action(rec);
            let outcome = rec.get("outcome").and_then(|x| x.as_str()).unwrap_or("");
            // The huge register/plan-show outcomes are dumps, not verdicts — keep
            // the line short by only showing a compact outcome.
            let short = compact_outcome(outcome);
            events.push(Event {
                ts: ts_to_bne(ts),
                marker: '⊙',
                text: format!(
                    "{action}{}",
                    if short.is_empty() {
                        String::new()
                    } else {
                        format!(" → {short}")
                    }
                ),
            });
        }
    }

    // Engine ticks that fired a rule.
    if let Some(ticks) = v.get("ticks").and_then(|t| t.as_array()) {
        for tick in ticks {
            let ts = tick.get("tick_ts").and_then(|x| x.as_str()).unwrap_or("");
            let fired = tick
                .get("eval")
                .and_then(|e| e.get("fired"))
                .and_then(|f| f.as_array());
            if let Some(fired) = fired {
                for rule in fired {
                    let rule_id = rule.as_str().unwrap_or("?");
                    events.push(Event {
                        ts: ts_to_bne(ts),
                        marker: '•',
                        text: format!("fired {rule_id}"),
                    });
                }
            }
        }
    }

    events.sort_by(|a, b| a.ts.cmp(&b.ts));
    events
}

/// The action of an inbound record, read from its signed body's `action:` line.
fn record_action(rec: &Value) -> String {
    let body = rec.get("body").and_then(|b| b.as_str()).unwrap_or("");
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("action:") {
            return rest.trim().to_string();
        }
    }
    rec.get("intent_id")
        .and_then(|x| x.as_str())
        .unwrap_or("record")
        .to_string()
}

/// Shorten an outcome string for a one-line event. Multi-line dumps (register /
/// plan-show responses) collapse to their first meaningful token.
fn compact_outcome(outcome: &str) -> String {
    let first = outcome.lines().next().unwrap_or("").trim();
    if first.len() > 60 {
        first[..60].to_string()
    } else {
        first.to_string()
    }
}

/// Derive the entry timestamp (Brisbane) — the ts of the first record whose
/// outcome indicates a fill (`entered`). `None` if the plan never entered.
pub fn derive_entry_ts(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    let records = v.get("records")?.as_array()?;
    for rec in records {
        let outcome = rec.get("outcome").and_then(|x| x.as_str()).unwrap_or("");
        if outcome.starts_with("entered") {
            let ts = rec.get("ts").and_then(|x| x.as_str()).unwrap_or("");
            return Some(ts_to_bne(ts));
        }
    }
    None
}

/// Derive the final outcome for the info bar: the last non-trivial record
/// outcome (`entered`, `rejected: …`, `closed …`). Returns `(text, is_ok)`
/// where `is_ok` drives green vs red. Falls back to the plan's phase when no
/// dispatch outcome is recorded.
pub fn derive_outcome(json: &str) -> (String, bool) {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return ("?".to_string(), false);
    };
    let mut result: Option<(String, bool)> = None;
    if let Some(records) = v.get("records").and_then(|r| r.as_array()) {
        for rec in records {
            let outcome = rec.get("outcome").and_then(|x| x.as_str()).unwrap_or("");
            // Skip the big dump outcomes (register/plan-show) — they start with a
            // YAML sequence or `ok`.
            if outcome == "ok" || outcome.starts_with("- ") || outcome.contains('\n') {
                continue;
            }
            if outcome.is_empty() {
                continue;
            }
            let ok = outcome.starts_with("entered") || outcome.starts_with("closed");
            result = Some((outcome.to_string(), ok));
        }
    }
    result.unwrap_or_else(|| ("no dispatch recorded".to_string(), false))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMELINE: &str = include_str!("../tests/fixtures/plan_timeline.json");

    #[test]
    fn parses_events_in_time_order() {
        let events = parse_events(TIMELINE);
        assert!(!events.is_empty());
        for w in events.windows(2) {
            assert!(w[0].ts <= w[1].ts, "events must be sorted by ts");
        }
    }

    #[test]
    fn ts_converts_to_brisbane() {
        // 2026-07-22T09:12:11Z → Brisbane +10 → 19:12.
        let out = ts_to_bne("2026-07-22T09:12:11.316625796+00:00");
        assert_eq!(out, "2026-07-22 19:12");
    }

    #[test]
    fn outcome_falls_back_without_dispatch() {
        // The AUD/CAD fixture has only register + plan-show records (dumps), so
        // no clean dispatch verdict — expect the fallback.
        let (text, ok) = derive_outcome(TIMELINE);
        assert!(!ok);
        assert_eq!(text, "no dispatch recorded");
    }
}
