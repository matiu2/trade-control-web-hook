use super::PositionLink;
use crate::{intent::Intent, state::StateStore};

/// Recording failure must never turn an accepted order into a rejection that
/// callers could retry. The broker has already accepted this placement.
pub async fn record_placement<S: StateStore>(store: &S, intent: &Intent, order_id: &str) {
    let (Some(trade_id), Some(direction)) = (&intent.trade_id, intent.direction) else {
        return;
    };
    if intent.dry_run.unwrap_or(false) || order_id.is_empty() {
        return;
    }
    let link = PositionLink {
        account: intent.account.clone(),
        trade_id: trade_id.clone(),
        instrument: intent.instrument.clone(),
        direction,
        broker_order_id: order_id.into(),
        broker_position_id: None,
    };
    if let Err(error) = store.record_position_link(link).await {
        tracing::error!(%error,trade_id,order_id,"accepted broker placement could not be linked for position reporting");
    }
}
