//! Broker lifecycle evidence shared by live reconciliation and replay.
use crate::{broker::AttemptState, intent::Direction};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    Execute,
    Open,
    Close,
    Cancel,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub kind: EventKind,
    pub id: String,
    pub instrument: String,
    pub direction: Direction,
    pub currency: String,
    pub stake: f64,
    pub at: DateTime<Utc>,
    pub price: Option<f64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub id: String,
    pub order_id: String,
    pub entry_price: Option<f64>,
    pub opened_at: Option<DateTime<Utc>>,
}
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub pending: Vec<String>,
    pub positions: Vec<Position>,
    pub events: Vec<Event>,
    pub history_complete: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub order_id: String,
    pub stored_position_id: Option<String>,
    pub resolved_position_id: Option<String>,
    pub state: String,
    pub reason: String,
    pub history_complete: bool,
    pub positions: Vec<Position>,
    pub candidates: Vec<Event>,
    pub execution: Option<Event>,
}
#[derive(Debug, Clone)]
pub struct Observation {
    pub state: AttemptState,
    pub diagnostic: Option<Diagnostic>,
}
impl Observation {
    pub fn plain(state: AttemptState) -> Self {
        Self {
            state,
            diagnostic: None,
        }
    }
}

/// Activity execution events are second-resolution. A 2-second window covers
/// the observed 1-second broker skew; it is NOT the 90-second transaction
/// rounding tolerance, nor a window measured from pending-order placement.
pub const EXECUTION_SKEW_SECONDS: i64 = 2;

pub fn position_state(
    id: &str,
    open: bool,
    exit: Option<crate::broker_exit::BrokerTradeExit>,
) -> AttemptState {
    if open {
        return AttemptState::OpenPosition {
            broker_trade_id: id.into(),
        };
    }
    match exit {
        Some(exit) => match exit.realized_pl.filter(|pl| pl.is_finite()) {
            Some(pl) => closed_with_pl(pl),
            None => AttemptState::ClosedConfirmed {
                exit: Box::new(exit),
            },
        },
        None => AttemptState::Unknown,
    }
}

/// Replay and broker adapters use the same P/L classification when available.
pub fn closed_with_pl(realized_pl: f64) -> AttemptState {
    if !realized_pl.is_finite() {
        return AttemptState::Unknown;
    }
    if realized_pl > 0.0 {
        AttemptState::ClosedWin { realized_pl }
    } else {
        AttemptState::ClosedLossOrBreakeven { realized_pl }
    }
}

pub fn resolve(order_id: &str, position_id: Option<&str>, evidence: &Evidence) -> Observation {
    let mut diagnostic = Diagnostic {
        order_id: order_id.into(),
        stored_position_id: position_id.map(str::to_owned),
        resolved_position_id: None,
        state: String::new(),
        reason: String::new(),
        history_complete: evidence.history_complete,
        positions: evidence.positions.clone(),
        candidates: Vec::new(),
        execution: None,
    };
    diagnostic.positions.sort_by(|a, b| a.id.cmp(&b.id));
    let (state, reason) = resolve_inner(order_id, position_id, evidence, &mut diagnostic);
    diagnostic.state = match &state {
        AttemptState::Pending => "pending",
        AttemptState::OpenPosition { .. } => "open",
        AttemptState::Cancelled => "cancelled",
        AttemptState::Unknown => "unknown",
        _ => "closed",
    }
    .into();
    diagnostic.reason = reason.into();
    Observation {
        state,
        diagnostic: Some(diagnostic),
    }
}

fn resolve_inner(
    order_id: &str,
    position_id: Option<&str>,
    e: &Evidence,
    d: &mut Diagnostic,
) -> (AttemptState, &'static str) {
    if e.pending.iter().any(|id| id == order_id) {
        return (AttemptState::Pending, "pending-order-id");
    }
    if let Some(p) = e.positions.iter().find(|p| {
        position_id == Some(p.id.as_str()) || (position_id.is_none() && p.order_id == order_id)
    }) {
        d.resolved_position_id = Some(p.id.clone());
        return (position_state(&p.id, true, None), "current-position-id");
    }
    if let Some(id) = position_id {
        d.resolved_position_id = Some(id.into());
        return (closed_state(id, e), "stored-position-id");
    }
    let events = unique_events(&e.events);
    let executions: Vec<_> = events
        .iter()
        .copied()
        .filter(|v| v.kind == EventKind::Execute && v.id == order_id)
        .collect();
    let [execution] = executions.as_slice() else {
        if executions.is_empty()
            && e.history_complete
            && events
                .iter()
                .any(|v| v.kind == EventKind::Cancel && v.id == order_id)
        {
            return (AttemptState::Cancelled, "explicit-order-cancellation");
        }
        return (AttemptState::Unknown, "execution-missing-or-conflicting");
    };
    d.execution = Some((*execution).clone());
    let candidates: Vec<_> = events
        .iter()
        .copied()
        .filter(|v| v.kind == EventKind::Open && potentially_compatible(execution, v))
        .collect();
    d.candidates = candidates.iter().map(|v| (*v).clone()).collect();
    d.candidates.sort_by(|a, b| a.id.cmp(&b.id));
    if !e.history_complete {
        return (AttemptState::Unknown, "incomplete-activity-history");
    }
    let [opened] = candidates.as_slice() else {
        return (AttemptState::Unknown, "position-missing-or-ambiguous");
    };
    if !compatible(execution, opened) {
        return (AttemptState::Unknown, "match-price-unavailable");
    }
    // Bidirectional uniqueness: two executions must not both claim one open.
    if events
        .iter()
        .filter(|v| v.kind == EventKind::Execute && potentially_compatible(v, opened))
        .count()
        != 1
    {
        return (AttemptState::Unknown, "competing-executions");
    }
    d.resolved_position_id = Some(opened.id.clone());
    if e.positions.iter().any(|p| p.id == opened.id) {
        return (
            position_state(&opened.id, true, None),
            "activity-unique-match",
        );
    }
    (closed_state(&opened.id, e), "activity-unique-match")
}

fn unique_events(events: &[Event]) -> Vec<&Event> {
    events.iter().fold(Vec::new(), |mut out, event| {
        if !out.contains(&event) {
            out.push(event);
        }
        out
    })
}
fn canonical(instrument: &str) -> String {
    instrument
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_uppercase)
        .collect()
}
fn potentially_compatible(execution: &Event, opened: &Event) -> bool {
    canonical(&execution.instrument) == canonical(&opened.instrument)
        && !execution.currency.is_empty()
        && execution.currency == opened.currency
        && execution.direction == opened.direction
        && execution.stake.is_finite()
        && execution.stake > 0.0
        && (execution.stake - opened.stake).abs() <= 1e-8
        && (execution.at - opened.at).num_milliseconds().abs() <= EXECUTION_SKEW_SECONDS * 1000
        && match (execution.price, opened.price) {
            // Missing price remains a possible competitor. Only the final
            // unique match is required to have both prices.
            (Some(a), Some(b)) => {
                a.is_finite() && b.is_finite() && a > 0.0 && b > 0.0 && (a - b).abs() <= a * 0.0005
            }
            _ => true,
        }
}
fn compatible(execution: &Event, opened: &Event) -> bool {
    execution.price.is_some() && opened.price.is_some() && potentially_compatible(execution, opened)
}
fn closed_state(id: &str, e: &Evidence) -> AttemptState {
    use crate::broker_exit::{BrokerTradeExit, ExitReason};
    let close = e
        .events
        .iter()
        .filter(|v| v.kind == EventKind::Close && v.id == id)
        .max_by_key(|v| v.at);
    position_state(
        id,
        false,
        close.map(|v| BrokerTradeExit {
            broker_trade_id: id.into(),
            exit_order_id: None,
            transaction_id: None,
            closed_at: Some(v.at),
            reason: ExitReason::Unknown,
            broker_reason: Some("activity-close-position".into()),
            entry_price: None,
            exit_price: v.price,
            expected_price: None,
            realized_pl: None,
            account_currency: Some(v.currency.clone()),
            units: None,
            quote_currency: None,
        }),
    )
}

mod observation;
pub use observation::observe;

#[cfg(test)]
mod tests;
