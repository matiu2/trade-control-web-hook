//! Stateful quote sampling for retained spread-blocked entries.

use chrono::{DateTime, Utc};

use super::{PromoteOutcome, StoredCheck, promote_stored_order, stored_order};
use crate::broker::Broker;
use crate::pending_lifecycle::{EnterConfigProvider, VerifiedSource};
use crate::spread_retry::Decision;
use crate::state::{HeldTradeRecord, StateStore};

pub(super) async fn tick<B: Broker, S: StateStore, P: EnterConfigProvider, V: VerifiedSource>(
    broker: &B,
    store: &S,
    cfg: &P,
    src: &V,
    record: &HeldTradeRecord,
    now: DateTime<Utc>,
) -> Result<PromoteOutcome, String> {
    let Some(mut order) = stored_order(store, &record.trade_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(PromoteOutcome::NothingParked);
    };
    let Some(mut recovery) = order.spread_recovery else {
        return Err("spread-recovery park has no recovery state".into());
    };
    if now >= order.drop_at {
        return drop_if_current(store, &record.trade_id, &order, "spread-retry-expired").await;
    }
    if (now - recovery.sampled_at).num_seconds() < crate::spread_retry::POLL_SECONDS as i64 {
        return Ok(PromoteOutcome::StillWaiting);
    }
    let quote = match broker.get_quote(&record.instrument).await {
        Ok(quote) => quote,
        Err(error) => {
            tracing::warn!(%error, trade_id = %record.trade_id, "spread-retry: no fresh quote; continuing to wait");
            return Ok(PromoteOutcome::StillWaiting);
        }
    };
    let decision = recovery.decide(&quote, now);
    tracing::info!(trade_id = %record.trade_id, at = %now, ?decision, spread = quote.spread(),
        decay_per_second = recovery.spread_decay, adverse_price_per_second = recovery.adverse_speed,
        chase_r = recovery.chase_r(recovery.executable(&quote)), "spread-retry: observed quote decision");
    if let Decision::Drop(reason) = decision {
        return drop_if_current(store, &record.trade_id, &order, reason).await;
    }
    let original = order.clone();
    order.spread_recovery = Some(recovery);
    let updated = store
        .compare_set_stored_orders(&record.trade_id, &[original], &[order])
        .await
        .map_err(|e| e.to_string())?;
    if !updated {
        return Ok(PromoteOutcome::StillWaiting);
    }
    if matches!(decision, Decision::Wait(_)) {
        return Ok(PromoteOutcome::StillWaiting);
    }
    promote_stored_order(
        broker,
        store,
        cfg,
        src,
        &record.trade_id,
        StoredCheck {
            clears_min_r: true,
            bar_time: Some(now),
            spread_hour_over: false,
            market_open: false,
        },
        now,
    )
    .await
}

async fn drop_if_current<S: StateStore>(
    store: &S,
    trade_id: &str,
    order: &super::StoredOrder,
    reason: &'static str,
) -> Result<PromoteOutcome, String> {
    let cleared = store
        .compare_set_stored_orders(trade_id, std::slice::from_ref(order), &[])
        .await
        .map_err(|e| e.to_string())?;
    Ok(if cleared {
        PromoteOutcome::Dropped(reason)
    } else {
        PromoteOutcome::StillWaiting
    })
}
