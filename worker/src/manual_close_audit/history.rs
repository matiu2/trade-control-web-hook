use chrono::Utc;
use serde_json::Value;
use trade_control_core::{broker::Broker, settlement::Settlement, state::EntryAttempt};
use trade_control_cron::BrokerHandle;

pub(super) enum Audit {
    Observed(Settlement),
    Pending(String),
    Unavailable(String),
}

pub(super) fn attempts(body: &Value) -> Result<Vec<EntryAttempt>, serde_json::Error> {
    serde_json::from_value(body["admin_snapshot"]["detached_attempts"].clone())
}

pub(super) async fn check(broker: &BrokerHandle, body: &Value) -> Audit {
    let attempts = match attempts(body) {
        Ok(attempts) if !attempts.is_empty() => attempts,
        Ok(_) => {
            return Audit::Unavailable(
                "no recorded broker order IDs to match against closed-trade history".into(),
            );
        }
        Err(error) => {
            return Audit::Unavailable(format!("cannot read detached broker attempts: {error}"));
        }
    };
    let Some(instrument) = body["admin_snapshot"]["plan"]["instrument"].as_str() else {
        return Audit::Unavailable("missing originating instrument".into());
    };
    let now = Utc::now();
    let since = attempts
        .iter()
        .map(|attempt| attempt.placed_at)
        .min()
        .unwrap_or(now);
    let orders: Vec<_> = attempts
        .iter()
        .map(|attempt| attempt.broker_order_id.clone())
        .collect();
    // Settlement reads each adapter's closed-trade ledger, including MT5 deals.
    let result = match broker {
        BrokerHandle::Oanda(b) => b.fetch_settlement(instrument, since, &orders, now).await,
        BrokerHandle::TradeNation(b) => b.fetch_settlement(instrument, since, &orders, now).await,
        BrokerHandle::Ibkr(b) => b.fetch_settlement(instrument, since, &orders, now).await,
        BrokerHandle::Mt5(b) => b.fetch_settlement(instrument, since, &orders, now).await,
    };
    match result {
        Ok(settlement) => Audit::Observed(scoped_history(settlement, &attempts)),

        Err(error) => Audit::Pending(format!("closed-trade history lookup failed: {error:?}")),
    }
}

fn scoped_history(mut settlement: Settlement, attempts: &[EntryAttempt]) -> Settlement {
    // Never attribute another trade on the same instrument to this plan.
    settlement.trades.retain(|trade| {
        attempts.iter().any(|attempt| {
            trade.broker_order_id.as_deref() == Some(attempt.broker_order_id.as_str())
                || attempt.broker_trade_id.as_deref() == Some(trade.broker_trade_id.as_str())
        })
    });
    let unmatched = attempts
        .iter()
        .filter(|attempt| {
            !settlement.trades.iter().any(|trade| {
                trade.broker_order_id.as_deref() == Some(attempt.broker_order_id.as_str())
                    || attempt.broker_trade_id.as_deref() == Some(trade.broker_trade_id.as_str())
            })
        })
        .count();
    if unmatched > 0 {
        settlement.warnings.push(format!(
            "history did not account for {unmatched} recorded orders; audit remains pending"
        ));
    }
    settlement
}

pub(super) fn summary(settlement: &Settlement) -> (&'static str, String) {
    let closed = settlement
        .trades
        .iter()
        .filter(|trade| !trade.is_open())
        .count();
    let open = settlement
        .trades
        .iter()
        .filter(|trade| trade.is_open())
        .count();
    let warnings = if settlement.warnings.is_empty() {
        String::new()
    } else {
        format!("; {}", settlement.warnings.join("; "))
    };
    let status = if open == 0 && closed > 0 && settlement.warnings.is_empty() {
        "confirmed"
    } else {
        "pending"
    };
    (
        status,
        format!(
            "broker history matched {closed} closed and {open} open trades to recorded order IDs{warnings}; operator closure remains accepted"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_core::settlement::SettledTrade;

    #[test]
    fn incomplete_or_open_history_stays_pending_and_never_invents_pnl() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer())
            .try_init()
            .ok();
        let mut settlement = Settlement {
            broker: "mt5".into(),
            fetched_at: Utc::now(),
            trades: vec![],
            ledger: vec![],
            warnings: vec![],
        };
        assert_eq!(summary(&settlement).0, "pending");
        settlement.trades.push(SettledTrade {
            broker_trade_id: "1".into(),
            broker_order_id: Some("2".into()),
            instrument: None,
            entry_price: None,
            exit_price: None,
            size: None,
            opened_at: None,
            closed_at: None,
            realized_pl: None,
            financing: None,
            currency: None,
        });
        assert_eq!(summary(&settlement).0, "pending");
        settlement.trades[0].closed_at = Some(Utc::now());
        assert_eq!(summary(&settlement).0, "confirmed");
        let attempts: Vec<EntryAttempt> = ["2", "missing"]
            .into_iter()
            .map(|order| {
                serde_json::from_value(serde_json::json!({
                    "trade_id":"plan", "account":"expired", "instrument":"AUDUSD", "attempt_no":1,
                    "broker_order_id":order, "direction":"long", "placed_at":Utc::now(),
                    "shell_time":Utc::now(), "expires_at":Utc::now()
                }))
                .unwrap()
            })
            .collect();
        settlement = scoped_history(settlement, &attempts);
        assert!(settlement.warnings[0].contains("1 recorded orders"));
        assert_eq!(summary(&settlement).0, "pending");
        assert!(settlement.total_realized_pl().is_none());
    }
}
