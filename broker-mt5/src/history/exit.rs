use super::*;
use trade_control_core::broker_exit::{BrokerTradeExit, ExitReason};

/// Preserve proven deal identity even when a fill closed before the first poll.
pub(crate) fn exit(
    symbol: &str,
    order: &str,
    position: Option<&str>,
    live: &Trades,
    history: &History,
    currency: &str,
) -> Option<BrokerTradeExit> {
    let order = order.parse::<u64>().ok()?;
    let origin = history
        .deals
        .iter()
        .find(|d| d.order == order && d.symbol == symbol && d.entry == 0 && d.deal_type <= 1)?;
    if position.is_some_and(|id| id != origin.position.to_string())
        || live.positions.iter().any(|p| p.ticket == origin.position)
    {
        return None;
    }
    let deals: Vec<_> = history
        .deals
        .iter()
        .filter(|d| d.position == origin.position && d.symbol == symbol)
        .collect();
    if !fully_closed(&deals) {
        return None;
    }
    let last = deals
        .iter()
        .filter(|d| matches!(d.entry, 1 | 3) && d.deal_type <= 1)
        .max_by_key(|d| d.created_at)?;
    Some(BrokerTradeExit {
        broker_trade_id: origin.position.to_string(),
        exit_order_id: Some(last.order.to_string()),
        transaction_id: Some(last.ticket.to_string()),
        closed_at: Some(last.created_at),
        // The web deal shape does not expose MT5's reason enum. A stop cannot
        // be inferred from its price proximity or from the sign of the profit.
        reason: ExitReason::Unknown,
        broker_reason: None,
        expected_price: None,
        entry_price: weighted(&deals, true),
        exit_price: weighted(&deals, false),
        realized_pl: Some(deals.iter().map(|d| net(d)).sum()),
        account_currency: Some(currency.into()),
        units: None,
        quote_currency: None,
    })
}
