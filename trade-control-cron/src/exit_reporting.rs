//! Durable closure reporting; it never changes the plan or its retry ledger.

use chrono::{DateTime, Utc};
use trade_control_core::{
    broker_exit::{BrokerExit, BrokerTradeExit, ExitReason},
    recording::{CronNote, CronNoteSeverity},
    state::EntryAttempt,
};

use crate::seam::CronEnv;

pub(crate) async fn already_recorded<C: CronEnv>(cron: &C, attempt: &EntryAttempt) -> bool {
    match cron
        .broker_exit_recorded(
            attempt.account.as_deref(),
            &attempt.trade_id,
            &attempt.broker_order_id,
        )
        .await
    {
        Ok(recorded) => recorded,
        Err(err) => {
            tracing::error!(
                "reconcile: exit-note read failed for {}: {err}; will retry",
                attempt.trade_id
            );
            // A failed read must not permanently suppress a closure. The
            // durable writer deduplicates a repeated observation.
            false
        }
    }
}

pub(crate) async fn record<C: CronEnv>(
    cron: &C,
    attempt: &EntryAttempt,
    execution: BrokerTradeExit,
    now: DateTime<Utc>,
) {
    let ts = execution.closed_at.unwrap_or(now).to_rfc3339();
    let exit = BrokerExit {
        broker_order_id: attempt.broker_order_id.clone(),
        attempt_no: attempt.attempt_no,
        direction: attempt.direction,
        pip_size: attempt.pip_size,
        execution,
    };
    let message = exit.summary_lines().join("\n");
    let note = CronNote {
        ts,
        trade_id: attempt.trade_id.clone(),
        account: attempt.account.clone(),
        source: "broker-exit".into(),
        severity: CronNoteSeverity::Info,
        message: message.clone(),
        broker_exit: Some(exit),
    };
    match cron.record_broker_exit(note).await {
        Ok(()) => tracing::info!("reconcile: plan={} {message}", attempt.trade_id),
        Err(err) => tracing::error!(
            "reconcile: exit-note write failed for {}: {err}; will retry",
            attempt.trade_id
        ),
    }
}

/// A broker that confirms a closed state but exposes no execution details
/// still gets one honest closure event. Do not call a win TP or a loss SL.
pub(crate) fn without_details(attempt: &EntryAttempt, realized_pl: f64) -> BrokerTradeExit {
    BrokerTradeExit {
        broker_trade_id: attempt
            .broker_trade_id
            .clone()
            .unwrap_or_else(|| "unavailable".into()),
        exit_order_id: None,
        transaction_id: None,
        closed_at: None,
        reason: ExitReason::Unknown,
        broker_reason: None,
        entry_price: None,
        exit_price: None,
        expected_price: None,
        realized_pl: Some(realized_pl),
        account_currency: None,
        units: None,
        quote_currency: None,
    }
}
