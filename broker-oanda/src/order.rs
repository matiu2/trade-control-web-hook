//! Decimal order payloads for every OANDA entry kind.
use crate::spec::Spec;
use oanda_client::orders::{
    LimitOrder, MarketOrder, Order, OrderPositionFill, OrderType, StopLossDetails, StopOrder,
    TakeProfitDetails, TimeInForce,
};
use trade_control_core::{
    broker::EntryRequest,
    intent::{Direction, ResolvedEntry},
};

pub(crate) fn build(req: &EntryRequest<'_>, spec: &Spec, units: f64) -> Order {
    let instrument = req.instrument.to_string();
    let units = spec.signed_units(units, req.direction == Direction::Long);
    let stop_loss_on_fill = Some(StopLossDetails {
        price: Some(spec.price(req.stop_loss)),
        distance: None,
        time_in_force: TimeInForce::Gtc,
    });
    let take_profit_on_fill = Some(TakeProfitDetails {
        price: spec.price(req.take_profit),
        time_in_force: TimeInForce::Gtc,
    });
    match req.entry {
        ResolvedEntry::Market { .. } => Order::Market(MarketOrder {
            r#type: OrderType::Market,
            instrument,
            units,
            time_in_force: TimeInForce::Fok,
            position_fill: OrderPositionFill::Default,
            stop_loss_on_fill,
            take_profit_on_fill,
            trailing_stop_loss_on_fill: None,
        }),
        ResolvedEntry::Stop { trigger_price } => Order::Stop(StopOrder {
            r#type: OrderType::Stop,
            instrument,
            units,
            price: spec.price(trigger_price),
            time_in_force: TimeInForce::Gtc,
            position_fill: OrderPositionFill::Default,
            stop_loss_on_fill,
            take_profit_on_fill,
            trailing_stop_loss_on_fill: None,
        }),
        ResolvedEntry::Limit { trigger_price } => Order::Limit(LimitOrder {
            r#type: OrderType::Limit,
            instrument,
            units,
            price: spec.price(trigger_price),
            time_in_force: TimeInForce::Gtc,
            position_fill: OrderPositionFill::Default,
            stop_loss_on_fill,
            take_profit_on_fill,
            trailing_stop_loss_on_fill: None,
        }),
    }
}

#[cfg(test)]
pub(crate) mod tests;
