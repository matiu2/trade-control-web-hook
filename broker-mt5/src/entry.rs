use crate::{Mt5Broker, contract, currency_conversion, mapping, sizing};
use mt5_data_source::web::trading::{Direction as Side, OrderKind, OrderRequest, SymbolInfo};
use trade_control_core::{
    broker::*,
    intent::{Direction, ResolvedEntry},
};

#[cfg(test)]
mod distance_tests;

pub(crate) async fn place(
    broker: &Mt5Broker,
    cap: f64,
    max_positions: u32,
    req: &EntryRequest<'_>,
    market_band: Option<MarketEntryBand>,
) -> Result<Placement, EntryError> {
    // Risk decisions must bypass the short-lived snapshots used by cron reads.
    let (account, trades) = broker
        .trader
        .risk_snapshot()
        .await
        .map_err(|_| EntryError::AccountFetch)?;
    if trades.positions.len() + trades.orders.len() >= max_positions as usize {
        return Err(EntryError::OpenPositionsCapExceeded);
    }
    // Avoid adding to or reversing an existing netted position owned by another plan.
    if trades.positions.iter().any(|p| p.symbol == req.instrument)
        || trades.orders.iter().any(|o| o.symbol == req.instrument)
    {
        return Err(EntryError::OpenPositionsCapExceeded);
    }
    let symbol = broker
        .trader
        .symbol(req.instrument)
        .await
        .map_err(|_| EntryError::OrderRejected)?;
    contract::validate_calc_mode(symbol.calc_mode)?;
    let quote = broker
        .trader
        .quote(req.instrument)
        .await
        .map_err(|_| EntryError::AccountFetch)?;
    let (kind, price) = entry_price(req, &symbol, quote.bid, quote.ask)?;
    let stop = align(req.stop_loss, &symbol)?;
    let take = align(req.take_profit, &symbol)?;
    validate_stops(req.direction, price, stop, take)?;
    let equity = account.balance
        + account.credit
        + trades
            .positions
            .iter()
            .map(|p| p.profit + p.swap + p.commission)
            .sum::<f64>();
    let rate =
        currency_conversion::rate(&broker.trader, &symbol.profit_currency, &account.currency)
            .await?;
    // Account/FX reads can take seconds. A market rescue needs its OWN fresh
    // executable quote and size, rather than the pending order's reference.
    let price = if let Some(band) = market_band {
        let fresh = broker
            .trader
            .quote(req.instrument)
            .await
            .map_err(|_| EntryError::AccountFetch)?;
        let (_, price) = entry_price(req, &symbol, fresh.bid, fresh.ask)?;
        let aligned_request = EntryRequest {
            on_oversize: req.on_oversize,
            setup_id: req.setup_id,
            stop_loss: stop,
            take_profit: take,
            instrument: req.instrument,
            direction: req.direction,
            entry: req.entry.clone(),
            risk: req.risk,
            dry_run: req.dry_run,
            contract_multiplier: req.contract_multiplier,
        };
        band.check(&aligned_request, price)?;
        price
    } else {
        price
    };
    let loss = contract::loss_per_lot(symbol.calc_mode, symbol.contract_size, price, stop, rate)?;
    let lots = sizing::lots(req.risk, equity, loss, cap, &symbol.rules)?;
    let order = OrderRequest {
        symbol: req.instrument.into(),
        side: match req.direction {
            Direction::Long => Side::Buy,
            Direction::Short => Side::Sell,
        },
        kind,
        lots,
        price: (kind != OrderKind::Market).then_some(price),
        stop_loss: Some(stop),
        take_profit: Some(take),
        comment: "trade-control".into(),
    };
    broker.trader.preview(&order).await.map_err(|e| {
        tracing::warn!(symbol = req.instrument, %e, "MT5 entry validation failed");
        EntryError::OrderRejectedWithDetail(format!("MT5 entry validation before submission: {e}"))
    })?;
    tracing::info!(symbol=req.instrument, %lots, price, stop, take, equity, loss_per_lot=loss, dry_run=req.dry_run, "MT5 entry sized");
    if req.dry_run {
        return Ok(Placement {
            order_id: "dry-run".into(),
            size: Some(lots.as_f64()),
            price: Some(price),
        });
    }
    let outcome = broker.trader.open(&order).await.map_err(|e| {
        tracing::error!(symbol = req.instrument, %e, "MT5 entry rejected before transmission");
        EntryError::OrderRejectedWithDetail(format!("MT5 entry rejected before transmission: {e}"))
    })?;
    mapping::placement(outcome, lots, price)
}
fn align(price: f64, s: &SymbolInfo) -> Result<f64, EntryError> {
    if !price.is_finite() || price <= 0.0 {
        return Err(EntryError::OrderRejected);
    }
    let tick = if s.tick_size > 0.0 {
        s.tick_size
    } else {
        s.point
    };
    Ok((price / tick).round() * tick)
}
fn entry_price(
    req: &EntryRequest<'_>,
    s: &SymbolInfo,
    bid: f64,
    ask: f64,
) -> Result<(OrderKind, f64), EntryError> {
    let market = if req.direction == Direction::Long {
        ask
    } else {
        bid
    };
    let (kind, price) = match req.entry {
        ResolvedEntry::Market { .. } => (OrderKind::Market, market),
        ResolvedEntry::Stop { trigger_price } => (OrderKind::Stop, align(trigger_price, s)?),
        ResolvedEntry::Limit { trigger_price } => (OrderKind::Limit, align(trigger_price, s)?),
    };
    let pending_kind = match kind {
        OrderKind::Stop => PendingEntryKind::Stop,
        OrderKind::Limit => PendingEntryKind::Limit,
        _ => return Ok((kind, price)),
    };
    let snapshot = PendingEntryDistance {
        kind: pending_kind,
        direction: req.direction,
        trigger_price: price,
        quote: Quote { bid, ask },
        minimum_distance: f64::from(s.rules.stops_points.max(0)) * s.point,
    };
    if !snapshot.is_valid() {
        return Err(EntryError::OrderRejected);
    }
    if snapshot.is_rejected() {
        tracing::warn!(
            instrument = req.instrument,
            direction = ?req.direction,
            ?kind,
            price,
            bid,
            ask,
            distance = snapshot.signed_distance(),
            min_distance = snapshot.minimum_distance,
            "MT5 pending entry blocked before transmission: trigger too close to market"
        );
        return Err(if snapshot.is_near() {
            EntryError::EntryWithinMinimumDistance(snapshot)
        } else {
            EntryError::EntryTooCloseToMarket
        });
    }
    Ok((kind, price))
}
pub(crate) fn validate_stops(
    side: Direction,
    entry: f64,
    sl: f64,
    tp: f64,
) -> Result<(), EntryError> {
    let valid = match side {
        Direction::Long => sl < entry && tp > entry,
        Direction::Short => sl > entry && tp < entry,
    };
    if valid {
        Ok(())
    } else {
        Err(EntryError::OrderRejected)
    }
}
