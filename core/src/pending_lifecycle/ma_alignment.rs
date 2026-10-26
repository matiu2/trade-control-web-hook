//! A persistent alignment hold must outlive the short spread recovery timeout.
use crate::{
    hold::HoldReason,
    state::{HeldTradeRecord, StateError, StateStore},
};

pub(super) async fn protect<S: StateStore>(
    store: &S,
    record: &mut HeldTradeRecord,
) -> Result<bool, StateError> {
    if record.cancelled_orders.is_empty() {
        return Ok(false);
    }
    let pauses = store.list_pauses_for_trade(&record.trade_id).await?;
    let alignment_episode = record.holders.contains(HoldReason::MaAlignment)
        || pauses
            .iter()
            .any(|p| p.blackout_id == crate::ma_cross::ALIGNMENT_PAUSE_ID);
    if !alignment_episode {
        return Ok(false);
    }
    let Some(expires_at) = pauses.iter().map(|p| p.expires_at).max() else {
        return Ok(false);
    };
    record.holders.hold(HoldReason::MaAlignment);
    record.expires_at = record.expires_at.max(expires_at);
    Ok(true)
}
