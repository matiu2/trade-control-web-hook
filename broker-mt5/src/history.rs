use chrono::{DateTime, Utc};
use mt5_data_source::web::trading::{Deal, History, Lots, Trades};
use trade_control_core::{broker::AttemptState, settlement::{Settlement, SettledTrade, LedgerEntry, LedgerSource}};
use std::collections::BTreeSet;

pub(crate) fn attempt(symbol: &str, order: &str, position: Option<&str>, live: &Trades, history: &History) -> AttemptState {
    let Ok(order) = order.parse::<u64>() else { return AttemptState::Unknown; };
    if live.orders.iter().any(|o| o.ticket == order && o.symbol == symbol) { return AttemptState::Pending; }
    let position = position.and_then(|p| p.parse().ok()).or_else(|| history.deals.iter().find(|d| d.order == order && d.symbol == symbol && d.entry == 0).map(|d| d.position));
    if let Some(id) = position {
        if live.positions.iter().any(|p| p.ticket == id && p.symbol == symbol) { return AttemptState::OpenPosition { broker_trade_id:id.to_string() }; }
        let rows: Vec<_> = history.deals.iter().filter(|d| d.position == id && d.symbol == symbol).collect();
        if fully_closed(&rows) {
            let pnl = rows.iter().map(|d| net(d)).sum();
            return if pnl > 0.0 { AttemptState::ClosedWin { realized_pl:pnl } } else { AttemptState::ClosedLossOrBreakeven { realized_pl:pnl } };
        }
    }
    if history.orders.iter().any(|o| o.ticket == order && o.symbol == symbol && matches!(o.state, 2 | 5 | 6)) { AttemptState::Cancelled } else { AttemptState::Unknown }
}
fn net(d: &Deal) -> f64 { d.profit+d.commission+d.fee+d.swap }
fn fully_closed(rows: &[&Deal]) -> bool {
    // INOUT reversals cannot be attributed to a single entry safely.
    if rows.iter().any(|d| d.entry == 2) { return false; }
    let opened: u128 = rows.iter().filter(|d| d.entry == 0 && d.deal_type <= 1).map(|d| u128::from(d.volume_raw)).sum();
    let closed: u128 = rows.iter().filter(|d| matches!(d.entry,1|3) && d.deal_type <= 1).map(|d| u128::from(d.volume_raw)).sum();
    opened > 0 && opened == closed
}
fn weighted(rows: &[&Deal], entry: bool) -> Option<f64> {
    let rows: Vec<_> = rows.iter().filter(|d| if entry { d.entry==0 } else { matches!(d.entry,1|3) }).collect();
    let volume: f64 = rows.iter().map(|d| d.volume_raw as f64).sum();
    (volume>0.0).then(|| rows.iter().map(|d| d.price*d.volume_raw as f64).sum::<f64>()/volume)
}
pub(crate) fn settlement(symbol: &str, orders: &[String], live: &Trades, history: &History, currency: &str, now: DateTime<Utc>) -> Settlement {
    let ids: BTreeSet<u64> = orders.iter().filter_map(|s| s.parse().ok()).collect();
    let positions: BTreeSet<_> = history.deals.iter().filter(|d| ids.contains(&d.order) && d.symbol == symbol && d.entry == 0).map(|d| d.position).collect();
    let rows: Vec<_> = history.deals.iter().filter(|d| positions.contains(&d.position) && d.symbol == symbol).collect();
    let trades = positions.iter().map(|id| {
        let deals: Vec<_> = rows.iter().copied().filter(|d| d.position==*id).collect();
        let closed = fully_closed(&deals) && !live.positions.iter().any(|p| p.ticket==*id);
        let entry = deals.iter().filter(|d| d.entry==0).min_by_key(|d| d.created_at);
        SettledTrade { broker_trade_id:id.to_string(), broker_order_id:entry.map(|d| d.order.to_string()), instrument:Some(symbol.into()), entry_price:weighted(&deals,true), exit_price:closed.then(|| weighted(&deals,false)).flatten(),
            size:Some(deals.iter().filter(|d| d.entry==0).map(|d| d.volume_raw as f64 / Lots::SCALE as f64).sum()), opened_at:entry.map(|d| d.created_at), closed_at:closed.then(|| deals.iter().map(|d| d.created_at).max()).flatten(),
            realized_pl:closed.then(|| deals.iter().map(|d| net(d)).sum()), financing:Some(deals.iter().map(|d| d.swap).sum()), currency:Some(currency.into()) }
    }).collect();
    let ledger = rows.iter().map(|d| LedgerEntry { source:LedgerSource::Activity, reference:Some(d.ticket.to_string()), occurred_at:Some(d.created_at), description:format!("MT5 deal type={} entry={} order={} position={} {}", d.deal_type, d.entry, d.order, d.position, d.comment), instrument:Some(d.symbol.clone()), price:Some(d.price), size:Some(d.volume_raw as f64 / Lots::SCALE as f64), amount:Some(net(d)), currency:Some(currency.into()) }).collect();
    let warnings = if positions.is_empty() { vec!["No MT5 entry deals found for the recorded order tickets".into()] } else { Vec::new() };
    Settlement { broker:"mt5".into(), fetched_at:now, trades, ledger, warnings }
}
