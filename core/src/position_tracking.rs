//! Observational position tracking, shared by live reporting and replay.
//! A symbol/direction match cannot prove ownership of a manual position.

use crate::{broker::OpenPosition, intent::Direction, state::EntryAttempt};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionObservation {
    pub link: PositionLink,
    /// None when the position filled and closed between observations.
    pub snapshot: Option<PositionSnapshot>,
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub status: PositionStatus,
    pub exit: Option<crate::broker_exit::BrokerTradeExit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionStatus {
    Open,
    Unconfirmed,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionSync {
    pub account: Option<String>,
    pub checked_at: chrono::DateTime<chrono::Utc>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionSnapshot {
    pub position: OpenPosition,
    /// Broker-reported floating P/L in account currency. Unknown stays unknown.
    pub unrealized_pl: Option<f64>,
    pub currency: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionLink {
    pub account: Option<String>,
    pub trade_id: String,
    pub instrument: String,
    pub direction: Direction,
    pub broker_order_id: String,
    pub broker_position_id: Option<String>,
}

impl From<&EntryAttempt> for PositionLink {
    fn from(attempt: &EntryAttempt) -> Self {
        Self {
            account: attempt.account.clone(),
            trade_id: attempt.trade_id.clone(),
            instrument: attempt.instrument.clone(),
            direction: attempt.direction,
            broker_order_id: attempt.broker_order_id.clone(),
            broker_position_id: attempt.broker_trade_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackedPosition {
    pub link: PositionLink,
    pub snapshot: PositionSnapshot,
}

/// Call with candidates from ONE broker account. Conflicting owners are
/// deliberately left unassigned; duplicate observations of one link coalesce.
pub fn match_positions(
    links: &[PositionLink],
    positions: &[PositionSnapshot],
) -> Vec<TrackedPosition> {
    positions
        .iter()
        .filter_map(|snapshot| {
            let mut candidates = links
                .iter()
                .filter(|link| matches(link, &snapshot.position));
            let link = candidates.next()?;
            if candidates
                .any(|other| other.account != link.account || other.trade_id != link.trade_id)
            {
                tracing::warn!(
                    position_id = snapshot.position.position_id,
                    "ambiguous position ownership; not assigning a trade"
                );
                return None;
            }
            Some(TrackedPosition {
                link: PositionLink {
                    broker_position_id: Some(snapshot.position.position_id.clone()),
                    ..link.clone()
                },
                snapshot: snapshot.clone(),
            })
        })
        .collect()
}

fn matches(link: &PositionLink, position: &OpenPosition) -> bool {
    link.instrument == position.instrument
        && link.direction == position.direction
        && match &link.broker_position_id {
            Some(id) => !id.is_empty() && *id == position.position_id,
            None => !link.broker_order_id.is_empty() && link.broker_order_id == position.order_id,
        }
}

#[cfg(test)]
mod tests;

mod observation;
pub use observation::{PositionCheck, observe};
mod placement;
pub use placement::record_placement;
