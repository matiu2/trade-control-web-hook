//! Observe entry fills for sound notifications without changing trading state.
mod baseline;
use crate::{PgStateStore, native_cron::NativeCronEnv, trade_notifications};
use chrono::{DateTime, Utc};
use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};
use trade_control_core::{
    broker::{AttemptState, Broker},
    recording::CronNote,
    state::{EntryAttempt, StateStore},
};
use trade_control_cron::{BrokerHandle, CronEnv};

fn key(attempt: &EntryAttempt, source: &str) -> String {
    trade_notifications::key(
        attempt.account.as_deref(),
        &attempt.trade_id,
        source,
        &attempt.broker_order_id,
    )
}

fn confirmed(attempt: &EntryAttempt, state: &AttemptState) -> Option<CronNote> {
    let detail = match state {
        AttemptState::OpenPosition { broker_trade_id } => format!("position={broker_trade_id}"),
        AttemptState::ClosedWin { .. }
        | AttemptState::ClosedLossOrBreakeven { .. }
        | AttemptState::ClosedConfirmed { .. } => {
            "entry confirmed from broker history; position already closed".into()
        }
        AttemptState::Pending | AttemptState::Cancelled | AttemptState::Unknown => return None,
    };
    Some(trade_notifications::note(
        attempt.account.as_deref(),
        &attempt.trade_id,
        "trade-entered",
        Utc::now(),
        format!(
            "{} {:?} entry confirmed: order={} {detail}",
            attempt.instrument, attempt.direction, attempt.broker_order_id
        ),
    ))
}

pub(crate) async fn poll(store: &PgStateStore, cron: &NativeCronEnv) {
    if let Err(error) = poll_inner(store, cron).await {
        tracing::warn!(%error, "entry notification observation failed; will retry");
    }
}

async fn poll_inner(store: &PgStateStore, cron: &NativeCronEnv) -> color_eyre::Result<()> {
    let activation = baseline::mark(store.pool(), "entry-observer-activation").await?;
    let attempts = store.list_all_entry_attempts().await?;
    let attempts: Vec<_> = attempts
        .into_iter()
        .filter(|a| {
            !a.superseded && !a.broker_order_id.starts_with("dry-run") && a.expires_at >= activation
        })
        .collect();
    let keys: Vec<_> = attempts
        .iter()
        .flat_map(|a| [key(a, "trade-entered"), key(a, "entry-baselined")])
        .collect();
    let reported: HashSet<String> =
        sqlx::query_scalar("SELECT event_key FROM operator_alert_outbox WHERE event_key = ANY($1) UNION SELECT marker_key FROM operator_notification_markers WHERE marker_key = ANY($1)")
            .bind(&keys)
            .fetch_all(store.pool())
            .await?
            .into_iter()
            .collect();
    let mut accounts: BTreeMap<Option<String>, Vec<EntryAttempt>> = BTreeMap::new();
    for attempt in attempts.into_iter().filter(|a| {
        !reported.contains(&key(a, "trade-entered"))
            && !reported.contains(&key(a, "entry-baselined"))
    }) {
        if let Some(id) = &attempt.broker_trade_id {
            record(
                store,
                &attempt,
                &AttemptState::OpenPosition {
                    broker_trade_id: id.clone(),
                },
                activation,
            )
            .await?;
        } else {
            accounts
                .entry(attempt.account.clone())
                .or_default()
                .push(attempt);
        }
    }
    for (account, attempts) in accounts {
        let broker = match tokio::time::timeout(
            Duration::from_secs(10),
            cron.acquire_engine_broker(account.as_deref()),
        )
        .await
        {
            Ok(Ok(broker)) => broker,
            Ok(Err(error)) => {
                tracing::warn!(?account, %error, "entry notification broker unavailable; will retry");
                continue;
            }
            Err(error) => {
                tracing::warn!(?account, %error, "entry notification broker acquisition timed out; will retry");
                continue;
            }
        };
        for attempt in attempts {
            match &broker {
                BrokerHandle::Oanda(b) => observe(store, b, &attempt, activation).await,
                BrokerHandle::TradeNation(b) => observe(store, b, &attempt, activation).await,
                BrokerHandle::Ibkr(b) => observe(store, b, &attempt, activation).await,
                BrokerHandle::Mt5(b) => observe(store, b, &attempt, activation).await,
            }
        }
    }
    Ok(())
}

async fn observe<B: Broker>(
    store: &PgStateStore,
    broker: &B,
    attempt: &EntryAttempt,
    activation: DateTime<Utc>,
) {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        broker.lookup_attempt_state(&attempt.instrument, &attempt.broker_order_id, None),
    )
    .await;
    match result {
        Ok(Ok(state)) => {
            if let Err(error) = record(store, attempt, &state, activation).await {
                tracing::error!(trade_id=attempt.trade_id, %error, "entry notification could not be queued; will retry");
            }
        }
        other => {
            tracing::warn!(trade_id=attempt.trade_id, error=?other, "entry fill lookup unavailable; will retry")
        }
    }
}

async fn record(
    store: &PgStateStore,
    attempt: &EntryAttempt,
    state: &AttemptState,
    activation: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    if matches!(state, AttemptState::Pending) {
        baseline::mark(store.pool(), &key(attempt, "entry-pending")).await?;
    } else if matches!(state, AttemptState::Cancelled) {
        baseline::mark(store.pool(), &key(attempt, "entry-baselined")).await?;
    } else if let Some(note) = confirmed(attempt, state) {
        if baseline::may_sound(store.pool(), attempt, activation).await? {
            trade_notifications::record(store.pool(), &attempt.broker_order_id, &note).await?;
        } else {
            // Existing fills are a baseline, not fresh entry events. Retain
            // this decision across restarts without sending old chart alerts.
            baseline::mark(store.pool(), &key(attempt, "entry-baselined")).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_broker_confirmed_fills_get_an_entry_sound_event() {
        let attempt: EntryAttempt = serde_json::from_value(serde_json::json!({
            "trade_id":"gbpaud", "account":"competition", "instrument":"GBPAUD", "attempt_no":1,
            "broker_order_id":"order-1", "direction":"short", "placed_at":"2026-10-16T01:30:00Z",
            "shell_time":"2026-10-16T01:30:00Z", "expires_at":"2026-10-17T01:30:00Z"
        }))
        .unwrap();
        [
            AttemptState::Pending,
            AttemptState::Unknown,
            AttemptState::Cancelled,
        ]
        .iter()
        .for_each(|state| assert!(confirmed(&attempt, state).is_none()));
        let note = confirmed(
            &attempt,
            &AttemptState::OpenPosition {
                broker_trade_id: "position-1".into(),
            },
        )
        .unwrap();
        assert_eq!(note.source, "trade-entered");
        assert!(note.message.contains("position-1"));
        let historical = confirmed(
            &attempt,
            &AttemptState::ClosedLossOrBreakeven { realized_pl: -1.0 },
        )
        .unwrap();
        assert!(historical.message.contains("already closed"));
    }
}
