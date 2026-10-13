use super::*;

/// The observation boundary used by both the retry gate and fill upkeep.
/// Persistence failures remain visible and are retried on the next observation.
pub async fn observe<B: crate::broker::Broker, S: crate::state::StateStore>(
    broker: &B,
    store: &S,
    attempt: &crate::state::EntryAttempt,
    now: DateTime<Utc>,
) -> Result<AttemptState, crate::broker::LookupError> {
    let observed = broker
        .lookup_attempt(
            &attempt.instrument,
            &attempt.broker_order_id,
            attempt.broker_trade_id.as_deref(),
        )
        .await;
    let observation = match &observed {
        Ok(observation) => observation.clone(),
        Err(_) => Observation {
            state: AttemptState::Unknown,
            diagnostic: Some(Diagnostic {
                order_id: attempt.broker_order_id.clone(),
                stored_position_id: attempt.broker_trade_id.clone(),
                resolved_position_id: None,
                state: "lookup-failed".into(),
                reason: "broker-transient".into(),
                history_complete: false,
                positions: Vec::new(),
                candidates: Vec::new(),
                execution: None,
            }),
        },
    };
    let recovered_id = match &observation.state {
        AttemptState::OpenPosition { broker_trade_id } => Some(broker_trade_id.as_str()),
        AttemptState::ClosedConfirmed { exit } => Some(exit.broker_trade_id.as_str()),
        _ => None,
    };
    if attempt.broker_trade_id.is_none()
        && let Some(id) = recovered_id
        && let Err(err) = store
            .set_entry_attempt_broker_trade_id(
                attempt.account.as_deref(),
                &attempt.trade_id,
                attempt.attempt_no,
                id,
            )
            .await
    {
        tracing::error!(trade_id = %attempt.trade_id, order_id = %attempt.broker_order_id, position_id = id, %err, "attempt identity persistence failed");
    }
    if let Some(diagnostic) = &observation.diagnostic {
        match store
            .record_attempt_diagnostic(attempt, diagnostic, now)
            .await
        {
            Ok(true) => {
                tracing::info!(trade_id = %attempt.trade_id, account = ?attempt.account, evidence = ?diagnostic, "attempt-resolution")
            }
            Ok(false) => {}
            Err(err) => {
                tracing::error!(trade_id = %attempt.trade_id, %err, evidence = ?diagnostic, "attempt-resolution journal write failed")
            }
        }
    }
    observed.map(|observation| observation.state)
}
