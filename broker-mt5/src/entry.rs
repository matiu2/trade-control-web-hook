use crate::{Mt5Broker, mapping, sizing};
use mt5_data_source::web::trading::{Direction as Side, OrderKind, OrderRequest, SymbolInfo, Trader};
use trade_control_core::{broker::*, intent::{Direction, ResolvedEntry}};

pub(crate) async fn place(broker: &Mt5Broker, cap: f64, max_positions: u32, req: &EntryRequest<'_>) -> Result<Placement, EntryError> {
    let account = broker.trader.account().await.map_err(|_| EntryError::AccountFetch)?;
    let trades = broker.trader.trades().await.map_err(|_| EntryError::AccountFetch)?;
    if trades.positions.len() + trades.orders.len() >= max_positions as usize { return Err(EntryError::OpenPositionsCapExceeded); }
    // Avoid adding to or reversing an existing netted position owned by another plan.
    if trades.positions.iter().any(|p| p.symbol == req.instrument) || trades.orders.iter().any(|o| o.symbol == req.instrument) { return Err(EntryError::OpenPositionsCapExceeded); }
    let symbol = broker.trader.symbol(req.instrument).await.map_err(|_| EntryError::OrderRejected)?;
    // FX linear contract P/L is verified. Refuse futures/options/bonds with different formulas.
    if !matches!(symbol.calc_mode, 0 | 5) { return Err(EntryError::ContractSizeUnavailable); }
    let quote = broker.trader.quote(req.instrument).await.map_err(|_| EntryError::AccountFetch)?;
    let (kind, price) = entry_price(req, &symbol, quote.bid, quote.ask)?;
    let stop = align(req.stop_loss, &symbol)?;
    let take = align(req.take_profit, &symbol)?;
    validate_stops(req.direction, price, stop, take)?;
    let equity = account.balance + account.credit + trades.positions.iter().map(|p| p.profit + p.swap + p.commission).sum::<f64>();
    let rate = currency_rate(&broker.trader, &symbol.profit_currency, &account.currency).await?;
    let loss = (price - stop).abs() * symbol.contract_size * rate;
    let lots = sizing::lots(req.risk, equity, loss, cap, &symbol.rules)?;
    let order = OrderRequest { symbol:req.instrument.into(), side:match req.direction { Direction::Long => Side::Buy, Direction::Short => Side::Sell }, kind, lots,
        price:(kind != OrderKind::Market).then_some(price), stop_loss:Some(stop), take_profit:Some(take), comment:"trade-control".into() };
    broker.trader.preview(&order).await.map_err(|e| { tracing::warn!(%e, "MT5 entry validation failed"); EntryError::OrderRejected })?;
    tracing::info!(symbol=req.instrument, %lots, price, stop, take, equity, loss_per_lot=loss, dry_run=req.dry_run, "MT5 entry sized");
    if req.dry_run { return Ok(Placement { order_id:"dry-run".into(), size:Some(lots.as_f64()), price:Some(price) }); }
    let outcome = broker.trader.open(&order).await.map_err(|e| { tracing::error!(%e, "MT5 entry rejected before transmission"); EntryError::OrderRejected })?;
    mapping::placement(outcome, lots, price)
}
fn align(price: f64, s: &SymbolInfo) -> Result<f64, EntryError> {
    if !price.is_finite() || price <= 0.0 { return Err(EntryError::OrderRejected); }
    let tick = if s.tick_size > 0.0 { s.tick_size } else { s.point };
    Ok((price / tick).round() * tick)
}
fn entry_price(req: &EntryRequest<'_>, s: &SymbolInfo, bid: f64, ask: f64) -> Result<(OrderKind, f64), EntryError> {
    let market = if req.direction == Direction::Long { ask } else { bid };
    let (kind, price) = match req.entry {
        ResolvedEntry::Market { .. } => (OrderKind::Market, market),
        ResolvedEntry::Stop { trigger_price } => (OrderKind::Stop, align(trigger_price,s)?),
        ResolvedEntry::Limit { trigger_price } => (OrderKind::Limit, align(trigger_price,s)?),
    };
    let distance = match (kind, req.direction) {
        (OrderKind::Stop, Direction::Long) | (OrderKind::Limit, Direction::Short) => price-market,
        (OrderKind::Stop, Direction::Short) | (OrderKind::Limit, Direction::Long) => market-price,
        _ => return Ok((kind,price)),
    };
    if distance <= 0.0 || distance < f64::from(s.rules.stops_points.max(0))*s.point { return Err(EntryError::EntryTooCloseToMarket); }
    Ok((kind,price))
}
pub(crate) fn validate_stops(side: Direction, entry: f64, sl: f64, tp: f64) -> Result<(), EntryError> {
    let valid = match side { Direction::Long => sl < entry && tp > entry, Direction::Short => sl > entry && tp < entry };
    if valid { Ok(()) } else { Err(EntryError::OrderRejected) }
}
/// Conservative cost conversion: paying quote currency uses ask; inverse uses bid.
async fn leg(trader: &Trader, from: &str, to: &str) -> Option<f64> {
    if from == to { return Some(1.0); }
    if let Ok(q) = trader.quote(&format!("{from}{to}")).await { return Some(q.ask); }
    trader.quote(&format!("{to}{from}")).await.ok().map(|q| 1.0/q.bid)
}
async fn currency_rate(trader: &Trader, from: &str, to: &str) -> Result<f64, EntryError> {
    let rate = if let Some(rate) = leg(trader,from,to).await { rate } else {
        leg(trader,from,"USD").await.ok_or(EntryError::AccountFetch)? * leg(trader,"USD",to).await.ok_or(EntryError::AccountFetch)?
    };
    if rate.is_finite() && rate > 0.0 { Ok(rate) } else { Err(EntryError::AccountFetch) }
}
