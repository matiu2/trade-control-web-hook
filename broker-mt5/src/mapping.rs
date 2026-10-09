use mt5_data_source::web::trading::{
    Direction as Side, History, Lots, Order, OutcomeState, Position, TradeOutcome,
};
use trade_control_core::{broker::*, intent::Direction};

pub(crate) fn direction(side: Side) -> Direction {
    match side {
        Side::Buy => Direction::Long,
        Side::Sell => Direction::Short,
    }
}
pub(crate) fn lookup_error(error: color_eyre::Report) -> LookupError {
    tracing::warn!(%error, "MT5 broker read failed");
    LookupError::Transient
}
pub(crate) fn position(p: &Position, history: &History) -> OpenPosition {
    let origin = history
        .deals
        .iter()
        .filter(|d| d.position == p.ticket && d.entry == 0 && d.deal_type <= 1)
        .min_by_key(|d| d.created_at);
    OpenPosition {
        instrument: p.symbol.clone(),
        direction: direction(p.side),
        stop_loss: (p.stop_loss > 0.0).then_some(p.stop_loss),
        take_profit: (p.take_profit > 0.0).then_some(p.take_profit),
        position_id: p.ticket.to_string(),
        order_id: origin.map(|d| d.order.to_string()).unwrap_or_default(),
        stake: p.lots.as_f64(),
        entry_price: Some(p.open_price),
        opened_at: Some(p.opened_at),
    }
}
pub(crate) fn pending(o: &Order) -> Option<PendingOrder> {
    (2..=5).contains(&o.order_type).then(|| PendingOrder {
        order_id: o.ticket.to_string(),
        instrument: o.symbol.clone(),
        direction: if o.order_type.is_multiple_of(2) {
            Direction::Long
        } else {
            Direction::Short
        },
        trigger: o.price,
        is_stop: o.order_type >= 4,
        stake: o.remaining_raw as f64 / Lots::SCALE as f64,
    })
}
pub(crate) fn mutation_succeeded(o: &TradeOutcome) -> bool {
    matches!(
        o.state,
        OutcomeState::Filled | OutcomeState::Completed | OutcomeState::Unchanged
    )
}
pub(crate) fn placement(
    outcome: TradeOutcome,
    requested: Lots,
    price: f64,
) -> Result<Placement, EntryError> {
    if outcome.state == OutcomeState::Unknown {
        return Err(EntryError::AmbiguousSuccess(format!(
            "MT5 request {}: {}",
            outcome.request_id,
            outcome.uncertainty.unwrap_or_default()
        )));
    }
    if outcome.state == OutcomeState::Rejected {
        return Err(EntryError::OrderRejected);
    }
    let result = outcome.result.ok_or_else(|| {
        EntryError::AmbiguousSuccess("MT5 accepted entry without order identity".into())
    })?;
    if result.order == 0 {
        return Err(EntryError::AmbiguousSuccess(
            "MT5 accepted entry without order ticket".into(),
        ));
    }
    let size = if outcome.state == OutcomeState::Placed {
        requested.as_f64()
    } else {
        result.volume_raw as f64 / Lots::SCALE as f64
    };
    Ok(Placement {
        order_id: result.order.to_string(),
        size: Some(size),
        price: Some(price),
    })
}

/// Core watchers amend using the originating order, which differs from the position ticket.
pub(crate) fn amend_position_ticket(
    id: u64,
    live: &mt5_data_source::web::trading::Trades,
    history: &History,
) -> Option<u64> {
    live.positions
        .iter()
        .find(|p| {
            p.ticket == id
                || history.deals.iter().any(|d| {
                    d.order == id
                        && d.position == p.ticket
                        && d.symbol == p.symbol
                        && d.entry == 0
                        && d.deal_type <= 1
                })
        })
        .map(|p| p.ticket)
}
