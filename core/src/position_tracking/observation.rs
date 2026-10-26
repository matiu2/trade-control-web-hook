use super::*;
use crate::{
    broker::{AttemptState, Broker, LookupError},
    broker_exit::{BrokerTradeExit, ExitReason},
};
use chrono::{DateTime, Utc};

pub struct PositionCheck {
    /// Captured before the broker book read, never after slow history lookups.
    pub observed_at: DateTime<Utc>,
    pub positions: Vec<TrackedPosition>,
    pub exits: Vec<(PositionLink, BrokerTradeExit)>,
    pub warnings: Vec<String>,
}

/// Same observational path for native reporting and the replay broker.
pub async fn observe<B: Broker>(
    broker: &B,
    account: &str,
    links: &mut [PositionLink],
    observed_at: DateTime<Utc>,
) -> Result<PositionCheck, LookupError> {
    let mut result = PositionCheck {
        observed_at,
        positions: Vec::new(),
        exits: Vec::new(),
        warnings: Vec::new(),
    };
    let snapshots = broker.position_snapshots(account).await?;
    for link in links.iter_mut() {
        if snapshots
            .iter()
            .any(|snapshot| super::matches(link, &snapshot.position))
        {
            continue;
        }
        match broker
            .lookup_trade_exit(
                &link.instrument,
                &link.broker_order_id,
                link.broker_position_id.as_deref(),
            )
            .await
        {
            Ok(Some(exit)) => {
                record_exit(&mut result, link, exit);
                continue;
            }
            Err(error) => {
                warn(&mut result, link, format!("exit lookup failed: {error:?}"));
                continue;
            }
            Ok(None) => {}
        }
        match broker
            .lookup_attempt_state(
                &link.instrument,
                &link.broker_order_id,
                link.broker_position_id.as_deref(),
            )
            .await
        {
            Ok(AttemptState::OpenPosition { broker_trade_id }) => {
                link.broker_position_id = Some(broker_trade_id)
            }
            Ok(AttemptState::ClosedConfirmed { exit }) => record_exit(&mut result, link, *exit),
            Ok(
                AttemptState::ClosedWin { realized_pl }
                | AttemptState::ClosedLossOrBreakeven { realized_pl },
            ) => {
                if let Some(id) = &link.broker_position_id {
                    record_exit(&mut result, link, unknown_exit(id, realized_pl));
                } else {
                    warn(
                        &mut result,
                        link,
                        "closed order has no broker position ID; cannot attribute its position"
                            .into(),
                    );
                }
            }
            Err(error) => warn(
                &mut result,
                link,
                format!("ownership/state lookup failed: {error:?}"),
            ),
            Ok(AttemptState::Pending | AttemptState::Cancelled | AttemptState::Unknown) => {}
        }
    }
    result.positions = match_positions(links, &snapshots);
    for snapshot in &snapshots {
        if links
            .iter()
            .any(|link| super::matches(link, &snapshot.position))
            && !result
                .positions
                .iter()
                .any(|p| p.snapshot.position.position_id == snapshot.position.position_id)
        {
            result.warnings.push(format!(
                "position {} has conflicting trade owners; left unassigned",
                snapshot.position.position_id
            ));
        }
    }
    Ok(result)
}

fn record_exit(result: &mut PositionCheck, link: &PositionLink, exit: BrokerTradeExit) {
    if exit.broker_trade_id.is_empty()
        || link
            .broker_position_id
            .as_ref()
            .is_some_and(|id| *id != exit.broker_trade_id)
    {
        warn(
            result,
            link,
            "broker exit position ID conflicts with tracking".into(),
        );
        return;
    }
    let link = PositionLink {
        broker_position_id: Some(exit.broker_trade_id.clone()),
        ..link.clone()
    };
    result.exits.push((link, exit));
}

fn warn(result: &mut PositionCheck, link: &PositionLink, message: String) {
    let message = format!("order {}: {message}", link.broker_order_id);
    tracing::warn!(trade_id = link.trade_id, "{message}");
    result.warnings.push(message);
}

pub(super) fn unknown_exit(id: &str, realized_pl: f64) -> BrokerTradeExit {
    BrokerTradeExit {
        broker_trade_id: id.into(),
        realized_pl: Some(realized_pl),
        reason: ExitReason::Unknown,
        closed_at: None,
        exit_order_id: None,
        transaction_id: None,
        broker_reason: None,
        entry_price: None,
        exit_price: None,
        expected_price: None,
        account_currency: None,
        units: None,
        quote_currency: None,
    }
}
