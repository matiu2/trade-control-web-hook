//! TradeNation's activity rows identify a closed PositionID exactly. Its cash
//! ledger has no matching position/order key, so do not assign a neighbouring
//! trade's P&L or guess TP/SL from a price comparison.

use trade_control_core::broker_exit::{BrokerTradeExit, ExitReason};
use tradenation_api::ActivityRecord;

pub(crate) fn from_activity(
    instrument: &str,
    trade_id: &str,
    activity: &[ActivityRecord],
) -> Option<BrokerTradeExit> {
    let close = activity
        .iter()
        .filter(|r| r.market == instrument)
        .filter(|r| {
            r.result
                .strip_prefix("Close Position:")
                .is_some_and(|id| id.trim() == trade_id)
        })
        .max_by_key(|r| r.transaction_date)?;
    Some(BrokerTradeExit {
        broker_trade_id: trade_id.into(),
        exit_order_id: None,
        transaction_id: None,
        closed_at: close.transaction_date.map(|t| t.to_utc()),
        reason: ExitReason::Unknown,
        broker_reason: Some(close.result.clone()),
        entry_price: None,
        exit_price: close.price,
        expected_price: None,
        realized_pl: None,
        account_currency: Some(close.currency.clone()),
        units: None,
        quote_currency: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_ids_must_match_exactly() {
        let row: ActivityRecord = serde_json::from_value(serde_json::json!({
            "Market":"AUD/CAD", "TransactionDate":"08/10/26 03:10:00", "ExpiryDate":"",
            "Channel":"System", "Direction":"Buy", "DirectionID":"True", "Stake":"1.0", "Price":"0.99308",
            "Type":"", "StopOrderPrice":"-", "LimitOrderPrice":"-", "QuoteMode":"", "GoodTill":"",
            "Result":"Close Position:25260", "Currency":"AUD", "IsRollingMarket":false
        })).unwrap();
        assert!(from_activity("AUD/CAD", "2526", std::slice::from_ref(&row)).is_none());
        let mut ours = row;
        ours.result = "Close Position:2526".into();
        let exit = from_activity("AUD/CAD", "2526", &[ours]).unwrap();
        assert_eq!(exit.exit_price, Some(0.99308));
        assert_eq!(exit.reason, ExitReason::Unknown);
        assert_eq!(exit.expected_price, None);
        assert_eq!(exit.realized_pl, None);
        assert_eq!(exit.units, None);
    }
}
