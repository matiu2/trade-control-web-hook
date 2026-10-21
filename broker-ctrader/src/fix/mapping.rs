use super::*;
use ctrader_fix::trading::{NewOrder, OrderKind, OrderStatus, Side};
use trade_control_core::intent::Direction;

fn normalized(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_uppercase)
        .collect()
}
pub(super) fn resolve<'a>(symbols: &'a [Symbol], name: &str) -> Result<&'a Symbol> {
    ensure!(!name.is_empty(), "cTrader FIX instrument is empty");
    if let Some(exact) = symbols.iter().find(|symbol| symbol.name == name) {
        return Ok(exact);
    }
    let target = normalized(name);
    let matches: Vec<_> = symbols
        .iter()
        .filter(|symbol| normalized(&symbol.name) == target)
        .collect();
    ensure!(
        matches.len() == 1,
        "cTrader FIX instrument is absent or ambiguous on this account"
    );
    Ok(matches[0])
}
fn symbol_name(symbols: &[Symbol], id: u64) -> Result<String> {
    symbols
        .iter()
        .find(|symbol| symbol.id == id)
        .map(|symbol| symbol.name.clone())
        .ok_or_else(|| eyre!("cTrader FIX symbol ID is absent from this account's catalog"))
}
fn quantity(position: &Position) -> Result<(Direction, f64)> {
    ensure!(
        !position.position_id.is_empty(),
        "cTrader FIX position has no identity"
    );
    let long = position.long_quantity;
    let short = position.short_quantity;
    ensure!(
        long.is_finite() && short.is_finite() && long >= 0.0 && short >= 0.0,
        "cTrader FIX position quantity is invalid"
    );
    match (long > 0.0, short > 0.0) {
        (true, false) => Ok((Direction::Long, long)),
        (false, true) => Ok((Direction::Short, short)),
        _ => Err(eyre!(
            "cTrader FIX position must contain exactly one nonzero side"
        )),
    }
}
pub(super) fn position(symbols: &[Symbol], p: &Position) -> Result<OpenPosition> {
    let (direction, stake) = quantity(p)?;
    ensure!(
        p.average_price.is_finite() && p.average_price > 0.0,
        "invalid cTrader FIX position price"
    );
    Ok(OpenPosition {
        instrument: symbol_name(symbols, p.symbol_id)?,
        direction,
        stop_loss: p.stop_loss,
        take_profit: p.take_profit,
        position_id: p.position_id.clone(),
        order_id: String::new(),
        stake,
        entry_price: Some(p.average_price),
        opened_at: None,
    })
}
pub(super) fn pending(symbols: &[Symbol], r: &ExecutionReport) -> Result<PendingOrder> {
    ensure!(
        matches!(r.status, OrderStatus::New | OrderStatus::PartiallyFilled),
        "cTrader FIX order is not pending"
    );
    let direction = match r.side {
        Some(Side::Buy) => Direction::Long,
        Some(Side::Sell) => Direction::Short,
        None => return Err(eyre!("cTrader FIX pending order lacks direction")),
    };
    let (trigger, is_stop) = match (r.limit_price, r.stop_price) {
        (Some(price), None) => (price, false),
        (None, Some(price)) => (price, true),
        _ => {
            return Err(eyre!(
                "cTrader FIX pending order lacks an unambiguous trigger"
            ));
        }
    };
    let stake = r
        .leaves_quantity
        .ok_or_else(|| eyre!("cTrader FIX pending order lacks remaining quantity"))?;
    ensure!(
        !r.order_id.is_empty()
            && trigger.is_finite()
            && trigger > 0.0
            && stake.is_finite()
            && stake > 0.0,
        "cTrader FIX pending order has invalid identity, price or quantity"
    );
    Ok(PendingOrder {
        order_id: r.order_id.clone(),
        instrument: symbol_name(
            symbols,
            r.symbol_id
                .ok_or_else(|| eyre!("pending order lacks symbol"))?,
        )?,
        direction,
        trigger,
        is_stop,
        stake,
    })
}
pub(super) fn close_order(p: &Position, id: String) -> Result<NewOrder> {
    let (direction, quantity) = quantity(p)?;
    Ok(NewOrder {
        client_order_id: id,
        symbol_id: p.symbol_id,
        side: match direction {
            Direction::Long => Side::Sell,
            Direction::Short => Side::Buy,
        },
        quantity,
        kind: OrderKind::Market,
        expires_at: None,
        position_id: Some(p.position_id.clone()),
    })
}
pub(super) fn confirmed_close(
    p: &Position,
    r: &ExecutionReport,
    remaining: &[Position],
) -> Result<()> {
    let request = close_order(p, "validation".into())?;
    ensure!(
        r.position_id.as_deref() == Some(p.position_id.as_str())
            && r.symbol_id == Some(p.symbol_id)
            && r.side == Some(request.side)
            && r.status == OrderStatus::Filled
            && r.exec_type == "F"
            && r.quantity == Some(request.quantity)
            && r.average_price
                .is_some_and(|price| price.is_finite() && price > 0.0)
            && r.cumulative_quantity == Some(request.quantity)
            && r.leaves_quantity == Some(0.0),
        "cTrader FIX full close execution not confirmed; reconcile before retry"
    );
    ensure!(
        !remaining
            .iter()
            .any(|actual| actual.position_id == p.position_id),
        "cTrader FIX position remains after close; reconcile before retry"
    );
    Ok(())
}
pub(super) fn attempt(
    symbol: u64,
    order_id: &str,
    position_id: Option<&str>,
    orders: &[ExecutionReport],
    positions: &[Position],
) -> Result<AttemptState> {
    let order = orders.iter().find(|order| order.order_id == order_id);
    if let Some(order) = order {
        ensure!(
            order.symbol_id == Some(symbol),
            "cTrader FIX attempt order belongs to another instrument"
        );
    }
    let linked = order.and_then(|order| order.position_id.as_deref());
    ensure!(
        position_id.zip(linked).is_none_or(|(a, b)| a == b),
        "cTrader FIX attempt links conflicting positions"
    );
    if let Some(id) = position_id.or(linked)
        && let Some(position) = positions.iter().find(|position| position.position_id == id)
    {
        ensure!(
            position.symbol_id == symbol,
            "cTrader FIX attempt position belongs to another instrument"
        );
        quantity(position)?;
        return Ok(AttemptState::OpenPosition {
            broker_trade_id: id.into(),
        });
    }
    if let Some(order) = order {
        match order.status {
            OrderStatus::New | OrderStatus::PartiallyFilled => {
                ensure!(
                    order.leaves_quantity.is_some_and(|qty| qty > 0.0),
                    "cTrader FIX pending attempt has no remaining quantity"
                );
                return Ok(AttemptState::Pending);
            }
            // A partially filled cancellation is not proof the position is closed.
            OrderStatus::Cancelled | OrderStatus::Expired | OrderStatus::Rejected
                if order.cumulative_quantity == Some(0.0) =>
            {
                return Ok(AttemptState::Cancelled);
            }
            _ => {}
        }
    }
    Ok(AttemptState::Unknown)
}
