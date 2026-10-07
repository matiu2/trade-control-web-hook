//! Exit reasons come from closing transactions, prices from the exit order
//! and the closed trade. P&L alone cannot identify a TP or SL.

use chrono::{DateTime, Utc};
use oanda_client::{
    OandaClient,
    trades::{Trade, TradeState},
    transactions::{OrderFill, Transaction},
};
use trade_control_core::{
    broker::LookupError,
    broker_exit::{BrokerTradeExit, ExitReason},
};

pub async fn lookup(
    client: &OandaClient,
    account: &str,
    instrument: &str,
    order_id: &str,
    trade_id: Option<&str>,
) -> Result<Option<BrokerTradeExit>, LookupError> {
    let trade_id = match trade_id {
        Some(id) => id.to_string(),
        None => {
            let order = client
                .get_order(account, order_id)
                .await
                .map_err(transient)?;
            let Some(fill_id) = order.order.filling_transaction_id else {
                return Ok(None);
            };
            let transaction = client
                .get_transaction(account, &fill_id)
                .await
                .map_err(transient)?;
            let Transaction::Fill(fill) = transaction else {
                return Ok(None);
            };
            let Some(opened) = fill.trade_opened else {
                return Ok(None);
            };
            opened.trade_id
        }
    };
    let trade = client
        .get_trade(account, &trade_id)
        .await
        .map_err(transient)?;
    if trade.instrument != instrument || !matches!(trade.state, TradeState::Closed) {
        return Ok(None);
    }
    let mut fills = Vec::new();
    for id in trade.closing_transaction_ids.as_deref().unwrap_or_default() {
        if let Transaction::Fill(fill) = client
            .get_transaction(account, id)
            .await
            .map_err(transient)?
        {
            fills.push(fill);
        }
    }
    let fill = fills.iter().max_by(|a, b| a.time.cmp(&b.time));
    let expected_price = if let Some(fill) = fill
        && matches!(
            reason(fill.reason.as_deref()),
            ExitReason::TakeProfit | ExitReason::StopLoss | ExitReason::TrailingStopLoss
        )
        && let Some(id) = &fill.order_id
    {
        let order = client.get_order(account, id).await.map_err(transient)?;
        order
            .order
            .price
            .as_deref()
            .and_then(|p| p.parse::<f64>().ok())
    } else {
        None
    };
    // Account currency is metadata only; a temporary failure must not discard
    // the actual closing transaction or the price comparison.
    let currency = match client.get_account(account).await {
        Ok(response) => Some(response.account.currency),
        Err(err) => {
            tracing::info!("oanda exit account currency unavailable: {err:?}");
            None
        }
    };
    let report = from_trade(&trade, fill, expected_price, currency)?;
    Ok(Some(report))
}

fn transient(err: impl std::fmt::Debug) -> LookupError {
    tracing::error!("oanda exit lookup: {err:?}");
    LookupError::Transient
}

fn reason(reason: Option<&str>) -> ExitReason {
    match reason {
        Some("TAKE_PROFIT_ORDER") => ExitReason::TakeProfit,
        Some("STOP_LOSS_ORDER" | "GUARANTEED_STOP_LOSS_ORDER") => ExitReason::StopLoss,
        Some("TRAILING_STOP_LOSS_ORDER") => ExitReason::TrailingStopLoss,
        Some(_) => ExitReason::Other,
        None => ExitReason::Unknown,
    }
}

fn from_trade(
    trade: &Trade,
    fill: Option<&OrderFill>,
    expected_price: Option<f64>,
    currency: Option<String>,
) -> Result<BrokerTradeExit, LookupError> {
    let closed_at = trade
        .close_time
        .as_deref()
        .and_then(|t| t.parse::<DateTime<Utc>>().ok())
        .ok_or(LookupError::Transient)?;
    let single_exit = trade
        .closing_transaction_ids
        .as_deref()
        .is_some_and(|ids| ids.len() == 1);
    Ok(BrokerTradeExit {
        broker_trade_id: trade.id.clone(),
        exit_order_id: fill.and_then(|f| f.order_id.clone()),
        transaction_id: fill.map(|f| f.id.clone()),
        closed_at: Some(closed_at),
        reason: reason(fill.and_then(|f| f.reason.as_deref())),
        broker_reason: fill.and_then(|f| f.reason.clone()),
        entry_price: Some(trade.price),
        // With partial exits the average price is not the terminal fill. The
        // SDK doesn't preserve per-trade reductions, so leave the comparison
        // unavailable rather than labelling a blended price as slippage.
        exit_price: single_exit.then_some(trade.average_close_price).flatten(),
        expected_price: single_exit.then_some(expected_price).flatten(),
        realized_pl: Some(trade.realized_pl),
        account_currency: currency,
        units: single_exit
            .then(|| trade.initial_units.parse::<f64>().ok())
            .flatten(),
        quote_currency: trade
            .instrument
            .split_once('_')
            .map(|(_, quote)| quote.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade() -> Trade {
        serde_json::from_value(serde_json::json!({
            "id":"2526", "instrument":"AUD_CAD", "currentUnits":"0", "initialUnits":"-2416783",
            "price":"0.99078", "openTime":"2026-10-07T10:00:15Z", "state":"CLOSED",
            "initialMarginRequired":"100", "realizedPL":"-6169.1689", "financing":"0", "dividendAdjustment":"0",
            "averageClosePrice":"0.99308", "closeTime":"2026-10-07T17:10:00Z", "closingTransactionIDs":["2540"]
        })).unwrap()
    }

    #[test]
    fn broker_stop_is_reported_even_when_it_closed_at_a_profit() {
        let mut trade = trade();
        trade.realized_pl = 10.0;
        let fill: Transaction = serde_json::from_value(serde_json::json!({
            "id":"2540", "time":"2026-10-07T17:10:00Z", "type":"ORDER_FILL", "reason":"STOP_LOSS_ORDER", "orderID":"2539"
        })).unwrap();
        let Transaction::Fill(fill) = fill else {
            panic!("fixture is a fill")
        };
        let exit = from_trade(&trade, Some(&fill), Some(0.99324), Some("AUD".into())).unwrap();
        assert_eq!(exit.reason, ExitReason::StopLoss);
        assert_eq!(exit.broker_trade_id, "2526");
        assert_eq!(exit.transaction_id.as_deref(), Some("2540"));
        assert_eq!(exit.expected_price, Some(0.99324));
        assert_eq!(exit.exit_price, Some(0.99308));
        assert_eq!(exit.units, Some(-2416783.0));
    }

    #[test]
    fn reasons_are_not_guessed_from_profit() {
        assert_eq!(reason(Some("TAKE_PROFIT_ORDER")), ExitReason::TakeProfit);
        assert_eq!(reason(Some("MARKET_ORDER_TRADE_CLOSE")), ExitReason::Other);
        assert_eq!(reason(None), ExitReason::Unknown);
        assert_eq!(
            reason(Some("TRAILING_STOP_LOSS_ORDER")),
            ExitReason::TrailingStopLoss
        );
    }

    #[test]
    fn partial_exits_do_not_compare_average_price_to_final_trigger() {
        let mut trade = trade();
        trade.closing_transaction_ids = Some(vec!["2530".into(), "2540".into()]);
        let exit = from_trade(&trade, None, Some(0.99324), None).unwrap();
        assert_eq!(exit.expected_price, None);
        assert_eq!(exit.exit_price, None);
        assert_eq!(exit.units, None);
        assert_eq!(exit.realized_pl, Some(-6169.1689));
    }
}
