//! Parsing of the two plan shapes the journal reads:
//!
//! * [`PlanRow`] — one entry from `plan list --yaml` (the picker list).
//! * [`PlanDetail`] — the bare `TradePlan` from `plan export` (single-line
//!   JSON), reduced to the info-bar facts: entry mode, order type, direction,
//!   armed-at.
//!
//! We deliberately parse only the fields we render, using loose serde over
//! `serde_json::Value` / `serde_yaml::Value`, so a schema addition upstream
//! (the plan format evolves often) doesn't break the journal.

use color_eyre::eyre::{Result, eyre};
use serde::Deserialize;

/// One plan as summarised by `plan list --yaml`. Terminated (archived) plans
/// carry `archived_at`; live plans leave it `None`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PlanRow {
    pub trade_id: String,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub instrument: String,
    #[serde(default)]
    pub granularity: String,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub shadow: bool,
    #[serde(default)]
    pub archived_at: Option<String>,
    /// The engine's last processed bar time (RFC3339 UTC) — the plan's "last
    /// event". Absent for a plan that has never ticked. Used to order the list
    /// oldest-event-first so journalling works through the backlog in order.
    #[serde(default)]
    pub watermark: Option<String>,
}

impl PlanRow {
    /// True for a terminated / archived plan (only surfaced under
    /// `--include-all`).
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// The plan's last-event timestamp for ordering — the engine `watermark`.
    /// A plan that never ticked has none.
    pub fn last_event(&self) -> Option<&str> {
        self.watermark.as_deref()
    }
}

/// Parse the `plan list --yaml` output (a YAML sequence) into rows, ordered
/// **oldest last-event first** so the journalling backlog is worked in order.
/// Plans that never ticked (no `watermark`) sort last — they're not part of the
/// "what happened longest ago" queue.
pub fn parse_plan_list(yaml: &str) -> Result<Vec<PlanRow>> {
    // Empty / "no registered plans" bodies parse to an empty list.
    let trimmed = yaml.trim();
    if trimmed.is_empty() || trimmed.starts_with("no registered plans") {
        return Ok(Vec::new());
    }
    let mut rows: Vec<PlanRow> =
        serde_yaml::from_str(yaml).map_err(|e| eyre!("parse plan list YAML: {e}"))?;
    sort_oldest_event_first(&mut rows);
    Ok(rows)
}

/// Sort rows ascending by last-event timestamp; watermark-less plans go last.
/// RFC3339 UTC strings sort lexicographically in time order, so a string
/// compare is correct without parsing.
fn sort_oldest_event_first(rows: &mut [PlanRow]) {
    rows.sort_by(|a, b| match (a.last_event(), b.last_event()) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less, // has event → before the none
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.trade_id.cmp(&b.trade_id), // stable tiebreak
    });
}

/// How the trade enters — classified from which enter rules the plan carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryMode {
    /// `05-enter` only — the H&S break-and-close-then-retest stop entry.
    Normal,
    /// `09-enter-qm` only — the Quasimodo confirmed-candle entry.
    Quasimodo,
    /// Both `05-enter` and `09-enter-qm` — strategy-v2 runs both legs.
    StrategyV2,
    /// No recognised enter rule (e.g. a control-only or malformed plan).
    Unknown,
}

impl EntryMode {
    pub fn label(&self) -> &'static str {
        match self {
            EntryMode::Normal => "normal (break+close+retest)",
            EntryMode::Quasimodo => "quasimodo (confirmed candle)",
            EntryMode::StrategyV2 => "strategy-v2 (BCR + QM)",
            EntryMode::Unknown => "unknown",
        }
    }
}

/// The order type of one enter leg, from the intent's `entry.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    Market,
    Stop,
    Limit,
    Unknown,
}

impl OrderType {
    fn from_str(s: &str) -> Self {
        match s {
            "market" => OrderType::Market,
            "stop" => OrderType::Stop,
            "limit" => OrderType::Limit,
            _ => OrderType::Unknown,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            OrderType::Market => "market",
            OrderType::Stop => "stop",
            OrderType::Limit => "limit",
            OrderType::Unknown => "?",
        }
    }
}

/// The info-bar facts distilled from `plan export`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanDetail {
    pub trade_id: String,
    pub instrument: String,
    pub direction: String,
    pub granularity: String,
    pub armed_at: Option<String>,
    pub entry_mode: EntryMode,
    /// Order type per enter leg, in rule order. One entry for normal/QM, two
    /// for strategy-v2 (BCR leg then QM leg).
    pub order_types: Vec<(String, OrderType)>,
}

/// Parse the single-line `plan export` JSON into the info-bar facts.
pub fn parse_plan_export(json: &str) -> Result<PlanDetail> {
    let v: serde_json::Value =
        serde_json::from_str(json.trim()).map_err(|e| eyre!("parse plan export JSON: {e}"))?;

    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let trade_id = s("trade_id");
    if trade_id.is_empty() {
        return Err(eyre!("plan export has no trade_id"));
    }

    let rules = v
        .get("rules")
        .and_then(|r| r.as_array())
        .ok_or_else(|| eyre!("plan export has no rules array"))?;

    // The enter rules by rule_id. `05-enter` = BCR/normal, `09-enter-qm` = QM.
    let mut has_bcr = false;
    let mut has_qm = false;
    let mut order_types: Vec<(String, OrderType)> = Vec::new();
    for rule in rules {
        let rule_id = rule.get("rule_id").and_then(|x| x.as_str()).unwrap_or("");
        let is_bcr = rule_id == "05-enter";
        let is_qm = rule_id == "09-enter-qm";
        if !(is_bcr || is_qm) {
            continue;
        }
        if is_bcr {
            has_bcr = true;
        }
        if is_qm {
            has_qm = true;
        }
        let ot = rule
            .get("intent")
            .and_then(|i| i.get("entry"))
            .and_then(|e| e.get("type"))
            .and_then(|t| t.as_str())
            .map(OrderType::from_str)
            .unwrap_or(OrderType::Unknown);
        let leg = if is_bcr { "BCR" } else { "QM" };
        order_types.push((leg.to_string(), ot));
    }

    let entry_mode = match (has_bcr, has_qm) {
        (true, true) => EntryMode::StrategyV2,
        (true, false) => EntryMode::Normal,
        (false, true) => EntryMode::Quasimodo,
        (false, false) => EntryMode::Unknown,
    };

    Ok(PlanDetail {
        trade_id,
        instrument: s("instrument"),
        direction: s("direction"),
        granularity: s("granularity"),
        armed_at: v
            .get("armed_at")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        entry_mode,
        order_types,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = include_str!("../tests/fixtures/plan_list.yaml");
    const EXPORT: &str = include_str!("../tests/fixtures/plan_export.json");

    #[test]
    fn parses_plan_list_rows() {
        let rows = parse_plan_list(LIST).expect("parse");
        assert!(!rows.is_empty());
        // The fixture contains this plan; find it (order is now watermark-sorted,
        // not file order).
        let gbp = rows
            .iter()
            .find(|r| r.trade_id == "ihs-gbp-usd-c0451533")
            .expect("gbp/usd plan present");
        assert_eq!(gbp.instrument, "GBP/USD");
        assert_eq!(gbp.granularity, "m15");
        assert!(!gbp.is_archived());
    }

    #[test]
    fn list_is_ordered_oldest_event_first() {
        let rows = parse_plan_list(LIST).expect("parse");
        // Watermarks appear in non-decreasing order; watermark-less plans last.
        let mut seen_none = false;
        let mut prev: Option<&str> = None;
        for r in &rows {
            match r.last_event() {
                Some(ts) => {
                    assert!(
                        !seen_none,
                        "a plan with an event follows a watermark-less one"
                    );
                    if let Some(p) = prev {
                        assert!(p <= ts, "watermarks must ascend: {p} then {ts}");
                    }
                    prev = Some(ts);
                }
                None => seen_none = true,
            }
        }
    }

    #[test]
    fn empty_list_is_ok() {
        assert!(parse_plan_list("").unwrap().is_empty());
        assert!(parse_plan_list("no registered plans\n").unwrap().is_empty());
    }

    #[test]
    fn parses_export_as_normal_stop_entry() {
        let d = parse_plan_export(EXPORT).expect("parse");
        assert_eq!(d.trade_id, "hs-aud-cad-a07622da");
        assert_eq!(d.instrument, "AUD_CAD");
        assert_eq!(d.direction, "short");
        assert_eq!(d.granularity, "h1");
        // This fixture has only `05-enter` → normal, a stop entry.
        assert_eq!(d.entry_mode, EntryMode::Normal);
        assert_eq!(d.order_types, vec![("BCR".to_string(), OrderType::Stop)]);
        assert!(d.armed_at.is_some());
    }
}
