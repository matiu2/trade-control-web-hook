//! Entry-path integration for opt-in spread recovery. The signed bracket stays fixed.

use chrono::{DateTime, Duration, Utc};

use super::{ActionResult, EntryOrigin};
use crate::broker::{Broker, Granularity};
use crate::incoming::Verified;
use crate::intent::{Resolved, ResolvedEntry};
use crate::order_control::{StoredOrder, StoredReason, park_order, stored_order};
use crate::spread_retry::{Decision, Recovery};
use crate::state::StateStore;

#[allow(clippy::too_many_arguments)]
pub(super) async fn park<B: Broker, S: StateStore>(
    broker: &B,
    store: &S,
    verified: &Verified,
    resolved: &Resolved,
    boundary: f64,
    raw_body: Option<&str>,
    granularity: Option<Granularity>,
    origin: &EntryOrigin,
    now: DateTime<Utc>,
) -> String {
    let Some(policy) = verified.intent.spread_retry.filter(|p| p.valid()) else {
        return String::new();
    };
    if origin.skips_retry_gate() || !verified.intent.effective_entry_dedup().needs_retry_gate() {
        return String::new();
    }
    if super::dedup_before_park(broker, store, verified, origin, now)
        .await
        .is_some()
    {
        return String::new();
    }
    let quote = match broker.get_quote(&resolved.instrument).await {
        Ok(quote) if quote.bid.is_finite() && quote.ask.is_finite() && quote.ask >= quote.bid => {
            quote
        }
        result => {
            tracing::warn!(?result, "spread-retry: no usable quote; not parking");
            return String::new();
        }
    };
    let ideal_entry = resolved.entry.reference_price();
    let recovery = match origin {
        EntryOrigin::SpreadRetry { recovery } => *recovery,
        _ => Recovery {
            policy,
            direction: resolved.direction,
            ideal_entry,
            stop_loss: resolved.stop_loss,
            take_profit: resolved.take_profit,
            boundary,
            min_r: resolved.min_r,
            started_at: now,
            sampled_at: now,
            last_entry: match resolved.direction {
                crate::intent::Direction::Long => quote.ask,
                crate::intent::Direction::Short => quote.bid,
            },
            last_spread: quote.spread(),
            spread_decay: 0.0,
            adverse_speed: 0.0,
        },
    };
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return String::new();
    };
    if let Ok(Some(old)) = stored_order(store, trade_id).await
        && old.shell_time != verified.shell.time
        && let Some(mut prior) = old.spread_recovery
        && now < old.drop_at
        && !matches!(prior.decide(&quote, now), Decision::Drop(_))
        && (prior.take_profit - prior.ideal_entry).abs() / prior.risk()
            > (recovery.take_profit - recovery.ideal_entry).abs() / recovery.risk()
    {
        tracing::info!(
            trade_id,
            "spread-retry: retaining the higher-R parked signal"
        );
        return String::new();
    }
    let signed_intent = match raw_body
        .map(str::to_owned)
        .map(Ok)
        .unwrap_or_else(|| serde_yaml::to_string(&verified.intent))
    {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(%error, "spread-retry: could not serialize entry");
            return String::new();
        }
    };
    let bar_seconds = granularity.map(Granularity::seconds);
    let deadline = recovery.started_at + Duration::seconds(i64::from(policy.max_age_secs));
    let drop_at =
        crate::order_control::drop_at(verified.intent.not_after, bar_seconds.unwrap_or(3600), now)
            .min(deadline);
    let order = StoredOrder {
        signed_intent,
        reason: StoredReason::SpreadRecovery,
        spread_recovery: Some(recovery),
        original_sl_distance: recovery.risk(),
        tp_distance: (recovery.take_profit - recovery.ideal_entry).abs(),
        min_r: resolved.min_r,
        stored_at: recovery.started_at,
        drop_at,
        shell_time: verified.shell.time,
        replaces: None,
        bar_seconds,
    };
    match park_order(
        store,
        trade_id,
        &resolved.instrument,
        verified.intent.account.as_deref(),
        verified.intent.pip_size.unwrap_or(0.0),
        order,
        verified.intent.not_after,
        now,
    )
    .await
    {
        Ok(()) => {
            tracing::info!(trade_id, %drop_at, ideal_entry, "spread-retry: parked blocked entry");
            format!("; stored until {drop_at} (spread-recovery)")
        }
        Err(error) => {
            tracing::error!(%error, trade_id, "spread-retry: park failed");
            String::new()
        }
    }
}

pub(super) async fn prepare<B: Broker>(
    broker: &B,
    verified: &Verified,
    resolved: &mut Resolved,
    origin: &EntryOrigin,
    now: DateTime<Utc>,
) -> Result<Option<f64>, ActionResult> {
    let EntryOrigin::SpreadRetry { recovery } = origin else {
        return Ok(None);
    };
    let Some(policy) = verified.intent.spread_retry.filter(|p| p.valid()) else {
        return Err(rejected("spread-retry-config-missing"));
    };
    if policy != recovery.policy
        || recovery.ideal_entry != resolved.entry.reference_price()
        || recovery.stop_loss != resolved.stop_loss
        || recovery.take_profit != resolved.take_profit
    {
        return Err(rejected("spread-retry-config-mismatch"));
    }
    let quote = broker
        .get_quote(&resolved.instrument)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "spread-retry: fresh quote unavailable");
            rejected("spread-retry-no-quote")
        })?;
    let mut check = *recovery;
    match check.decide(&quote, now) {
        Decision::Ready => {}
        Decision::Wait(reason) | Decision::Drop(reason) => return Err(rejected(reason)),
    }
    let current = check.executable(&quote);
    let passed = match resolved.direction {
        crate::intent::Direction::Long => current >= recovery.ideal_entry,
        crate::intent::Direction::Short => current <= recovery.ideal_entry,
    };
    if passed {
        resolved.entry = ResolvedEntry::Market {
            reference_price: current,
        };
    }
    let entry = resolved.entry.reference_price();
    let risk = (entry - resolved.stop_loss).abs();
    if risk <= 0.0 || (resolved.take_profit - entry).abs() / risk < resolved.min_r {
        return Err(rejected("spread-retry-below-min-r"));
    }
    tracing::info!(
        ideal = recovery.ideal_entry,
        entry,
        spread = quote.spread(),
        chase_r = recovery.chase_r(current),
        "spread-retry: bounded recovery passed; rechecking all entry gates"
    );
    Ok(Some(quote.spread()))
}

fn rejected(reason: &str) -> ActionResult {
    ActionResult::Rejected {
        status: 422,
        body: reason.to_owned(),
        outcome: format!("rejected: {reason}"),
    }
}
