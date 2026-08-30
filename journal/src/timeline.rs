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
pub(crate) fn ts_to_bne(raw: &str) -> String {
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
                    // `eval.fired` entries are objects carrying `rule_id` (and the
                    // full fired intent); older shapes were bare strings, so
                    // handle both.
                    let rule_id = rule
                        .get("rule_id")
                        .and_then(|x| x.as_str())
                        .or_else(|| rule.as_str())
                        .unwrap_or("?");
                    let action = rule
                        .get("intent")
                        .and_then(|i| i.get("action"))
                        .and_then(|a| a.as_str());
                    let mut text = match action {
                        Some(a) => format!("fired {rule_id} ({a})"),
                        None => format!("fired {rule_id}"),
                    };
                    // What the fire actually DID at the broker. `eval.fired`
                    // carries only the *intent*; the outcome — including an
                    // entry's size and rate — rides the tick's separate
                    // `dispatch_outcomes` list, joined by `rule_id`.
                    if let Some(outcome) = dispatch_outcome(tick, rule_id) {
                        text.push_str(&format!(" → {outcome}"));
                    }
                    events.push(Event {
                        ts: ts_to_bne(ts),
                        marker: '•',
                        text,
                    });
                }
            }
        }
    }

    events.sort_by(|a, b| a.ts.cmp(&b.ts));
    events
}

/// Render the broker's settlement, if the plan has been archived with one, as
/// display lines for the foot of the timeline.
///
/// Reads the `plan export` JSON (not the timeline's), since the settlement is
/// captured on the archived plan rather than on any tick. Returns an empty vec
/// for a live plan, an archive with no settlement, or an unparseable body —
/// the settlement is a bonus on top of the timeline, never a reason to fail it.
///
/// Money and prices are printed exactly as the broker reported them: a missing
/// figure renders `?`, never `0`.
pub fn settlement_lines(export_json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(export_json) else {
        return Vec::new();
    };
    // `plan export` may hand back a single detail or a list of matches.
    let details: Vec<&Value> = match v.as_array() {
        Some(items) => items.iter().collect(),
        None => vec![&v],
    };
    let Some(s) = details
        .iter()
        .find_map(|d| d.get("settlement").filter(|s| !s.is_null()))
    else {
        return Vec::new();
    };

    let mut out = vec!["Settlement".to_string()];

    let num = |v: Option<&Value>| -> String {
        v.and_then(Value::as_f64)
            .map(|n| format!("{n}"))
            .unwrap_or_else(|| "?".to_string())
    };
    let text =
        |v: Option<&Value>| -> String { v.and_then(Value::as_str).unwrap_or("?").to_string() };

    if let Some(trades) = s.get("trades").and_then(Value::as_array) {
        for t in trades {
            let closed = t
                .get("closed_at")
                .and_then(Value::as_str)
                .map(ts_to_bne)
                // A plan can archive on a terminal veto or the expiry clock
                // before its position closes, so this is a real state.
                .unwrap_or_else(|| "still open".to_string());
            out.push(format!(
                "  {} {} size={} entry={} exit={} pl={} {}",
                text(t.get("broker_trade_id")),
                text(t.get("instrument")),
                num(t.get("size")),
                num(t.get("entry_price")),
                num(t.get("exit_price")),
                num(t.get("realized_pl")),
                closed,
            ));
        }
    }

    if let Some(ledger) = s.get("ledger").and_then(Value::as_array) {
        for e in ledger {
            let when = e
                .get("occurred_at")
                .and_then(Value::as_str)
                .map(ts_to_bne)
                .unwrap_or_else(|| "?".to_string());
            out.push(format!(
                "  [{}] {} {} price={} amount={}",
                text(e.get("source")),
                when,
                text(e.get("description")),
                num(e.get("price")),
                num(e.get("amount")),
            ));
        }
    }

    // Warnings last and unabbreviated: they say what the numbers above can't
    // be trusted to mean (e.g. TN's cash rows carry no order id).
    if let Some(warnings) = s.get("warnings").and_then(Value::as_array) {
        for w in warnings.iter().filter_map(Value::as_str) {
            out.push(format!("  ! {w}"));
        }
    }

    out
}

/// The dispatch outcome for one fired rule on this tick, unwrapped for display.
///
/// A tick records `eval.fired` (what the engine decided) and
/// `dispatch_outcomes` (what happened when that decision reached the broker) as
/// two separate lists, joined by `rule_id`. Only the second carries an entry's
/// size and rate, so a timeline that reads only the first can never show them.
///
/// Returns `None` when the tick recorded no outcome for the rule — a shadow
/// plan dispatches nothing, and an older recorded tick predates the field.
fn dispatch_outcome(tick: &Value, rule_id: &str) -> Option<String> {
    let raw = tick
        .get("dispatch_outcomes")?
        .as_array()?
        .iter()
        .find(|o| o.get("rule_id").and_then(|r| r.as_str()) == Some(rule_id))?
        .get("outcome")?
        .as_str()?;
    let unwrapped = unwrap_outcome(raw);
    (!unwrapped.is_empty()).then(|| unwrapped.to_string())
}

/// Strip the `ActionResult` wrapper off a recorded outcome: `Ok(entered: …)` →
/// `entered: …`. The wrapper names the variant, which the line's own success or
/// failure wording already conveys; the payload is the part worth the width.
/// Anything not in that shape is passed through untouched.
fn unwrap_outcome(raw: &str) -> &str {
    for prefix in ["Ok(", "Failed(", "Rejected("] {
        if let Some(inner) = raw.strip_prefix(prefix)
            && let Some(inner) = inner.strip_suffix(')')
        {
            return inner;
        }
    }
    raw
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
    fn fired_rules_show_id_not_placeholder() {
        // `eval.fired` entries are objects with `rule_id`; the parser must read
        // that, not fall through to `?`.
        let events = parse_events(TIMELINE);
        let fires: Vec<&Event> = events.iter().filter(|e| e.marker == '•').collect();
        assert!(!fires.is_empty(), "fixture has fired rules");
        assert!(
            fires.iter().all(|e| !e.text.contains('?')),
            "every fire should resolve a rule_id, got: {:?}",
            fires.iter().map(|e| &e.text).collect::<Vec<_>>()
        );
        assert!(
            fires.iter().any(|e| e.text.contains("pause")),
            "the pause fire should surface its action"
        );
    }

    /// A tick whose enter fired AND reached the broker. The fixture on disk is
    /// a pause-only tick, so the enter path needs its own.
    fn entered_tick() -> String {
        serde_json::json!({
            "records": [],
            "ticks": [{
                "tick_ts": "2026-08-15T01:00:00Z",
                "eval": {
                    "fired": [{
                        "rule_id": "05-enter",
                        "intent": { "action": "enter" }
                    }]
                },
                "dispatch_outcomes": [{
                    "rule_id": "05-enter",
                    "intent_id": "hs-aud-cad-a07622da-enter",
                    "outcome": "Ok(entered: order=27187050 size=2.75 @ 0.98574)",
                    "seq": 0
                }]
            }]
        })
        .to_string()
    }

    #[test]
    fn an_entered_line_shows_the_size_and_rate() {
        // The operator's ask: the enter line must say how big the trade was and
        // at what rate, not just that a rule fired.
        let events = parse_events(&entered_tick());
        let fire = events
            .iter()
            .find(|e| e.marker == '•')
            .expect("the enter fired");
        assert!(
            fire.text.contains("size=2.75"),
            "the trade size must show, got: {}",
            fire.text
        );
        assert!(
            fire.text.contains("0.98574"),
            "the rate must show, got: {}",
            fire.text
        );
        assert!(
            fire.text.starts_with("fired 05-enter (enter)"),
            "the historic prefix must survive, got: {}",
            fire.text
        );
    }

    #[test]
    fn the_action_result_wrapper_is_stripped() {
        // `Ok(...)` names the variant, which the payload already conveys —
        // spending the terminal width on it helps nobody.
        let events = parse_events(&entered_tick());
        let fire = events.iter().find(|e| e.marker == '•').expect("fired");
        assert!(
            !fire.text.contains("Ok("),
            "the wrapper should not reach the screen, got: {}",
            fire.text
        );
        assert_eq!(unwrap_outcome("Failed(broker rejected)"), "broker rejected");
        assert_eq!(unwrap_outcome("Rejected(veto-active)"), "veto-active");
        // Anything not in that shape passes through untouched.
        assert_eq!(unwrap_outcome("entered: order=1"), "entered: order=1");
    }

    #[test]
    fn a_fire_with_no_recorded_outcome_renders_as_before() {
        // A shadow plan dispatches nothing, and an older recorded tick predates
        // `dispatch_outcomes`. Neither may grow a dangling arrow.
        let no_outcome = serde_json::json!({
            "records": [],
            "ticks": [{
                "tick_ts": "2026-08-15T01:00:00Z",
                "eval": { "fired": [{ "rule_id": "05-enter",
                                      "intent": { "action": "enter" } }] },
                // Shadow tick: fired, dispatched nothing.
                "dispatch_outcomes": []
            }]
        })
        .to_string();
        let events = parse_events(&no_outcome);
        let fire = events.iter().find(|e| e.marker == '•').expect("fired");
        assert_eq!(
            fire.text, "fired 05-enter (enter)",
            "with no outcome the line is exactly the historic shape"
        );
    }

    #[test]
    fn a_control_fire_shows_its_outcome_too() {
        // The join isn't enter-specific: the AUD/CAD fixture's pause/resume
        // fires all carry outcomes, and showing them is the same win.
        let events = parse_events(TIMELINE);
        let pause = events
            .iter()
            .find(|e| e.marker == '•' && e.text.contains("pause"))
            .expect("the fixture has a pause fire");
        assert!(
            pause.text.contains("→ pause-set"),
            "a control fire shows what it did, got: {}",
            pause.text
        );
    }

    /// A `plan export` body for an archived plan carrying a settlement.
    fn settled_export() -> String {
        serde_json::json!({
            "plan": { "trade_id": "hs-aud-cad-a07622da" },
            "archived_at": "2026-08-18T05:00:00Z",
            "settlement": {
                "broker": "tradenation",
                "fetched_at": "2026-08-18T05:00:00Z",
                "trades": [{
                    "broker_trade_id": "27187050",
                    "instrument": "AUD/CAD",
                    "entry_price": 0.98574,
                    "exit_price": 0.98120,
                    "size": 2.75,
                    "closed_at": "2026-08-18T03:00:00Z",
                    "realized_pl": 1.25
                }],
                "ledger": [{
                    "source": "activity",
                    "occurred_at": "2026-08-15T01:00:00Z",
                    "description": "Execute Order:26793941",
                    "price": 0.98574
                }],
                "warnings": ["cash-ledger rows carry only a RefID"]
            }
        })
        .to_string()
    }

    #[test]
    fn settlement_shows_the_real_fills_and_pl() {
        let lines = settlement_lines(&settled_export());
        let body = lines.join("\n");
        assert!(body.contains("Settlement"), "{body}");
        assert!(body.contains("size=2.75"), "the size shows: {body}");
        assert!(
            body.contains("entry=0.98574"),
            "the real fill shows: {body}"
        );
        assert!(body.contains("pl=1.25"), "the P&L shows: {body}");
        // The attribution caveat must travel with the numbers it qualifies.
        assert!(
            body.contains("! cash-ledger rows carry only a RefID"),
            "{body}"
        );
    }

    #[test]
    fn settlement_renders_the_ledger_rows_too() {
        let body = settlement_lines(&settled_export()).join("\n");
        assert!(
            body.contains("[activity] ") && body.contains("Execute Order:26793941"),
            "the raw broker row is what you read when the summary looks wrong: {body}"
        );
    }

    #[test]
    fn a_missing_figure_renders_as_unknown_never_zero() {
        // The whole point of the Option-heavy settlement type: an unreported
        // exit must not read as a close at 0.
        let export = serde_json::json!({
            "settlement": {
                "broker": "tradenation",
                "fetched_at": "2026-08-18T05:00:00Z",
                "trades": [{ "broker_trade_id": "27187050", "instrument": "AUD/CAD" }],
                "ledger": [],
                "warnings": []
            }
        })
        .to_string();
        let body = settlement_lines(&export).join("\n");
        assert!(body.contains("exit=?"), "unknown renders as ?, got: {body}");
        assert!(body.contains("pl=?"), "unknown renders as ?, got: {body}");
        assert!(
            !body.contains("=0 ") && !body.contains("=0\n"),
            "never substitute a zero: {body}"
        );
    }

    #[test]
    fn a_trade_still_open_at_archive_says_so() {
        // A plan archives on a terminal veto / expiry, which can precede the
        // position closing. Rendering a blank close time would hide that.
        let export = serde_json::json!({
            "settlement": {
                "broker": "tradenation",
                "fetched_at": "2026-08-18T05:00:00Z",
                "trades": [{ "broker_trade_id": "27187050", "instrument": "AUD/CAD" }],
                "ledger": [],
                "warnings": []
            }
        })
        .to_string();
        assert!(
            settlement_lines(&export).join("\n").contains("still open"),
            "an unclosed trade must say so"
        );
    }

    #[test]
    fn a_live_plan_renders_no_settlement_block() {
        // Nothing to show, and an empty "Settlement" header would imply a
        // fetch that found nothing.
        let live = serde_json::json!({ "plan": { "trade_id": "t1" } }).to_string();
        assert!(settlement_lines(&live).is_empty());
        // An unparseable body must also not panic or emit a stray header.
        assert!(settlement_lines("not json").is_empty());
    }

    #[test]
    fn a_list_shaped_export_still_finds_the_settlement() {
        // `plan export` can hand back a list of matching scopes.
        let listed = format!("[{}]", settled_export());
        assert!(
            settlement_lines(&listed)
                .join("\n")
                .contains("entry=0.98574")
        );
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
