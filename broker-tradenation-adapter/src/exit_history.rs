//! Attribute cash and protective-order executions to an already confirmed position close.
use crate::attempts::{activity_event, same_instrument};
use trade_control_core::{
    attempt_resolution::EventKind,
    broker_exit::{BrokerTradeExit, ExitReason},
};
use tradenation_api::{ActivityRecord, TransactionRecord};

pub(super) fn enrich(
    exit: &mut BrokerTradeExit,
    activity: &[ActivityRecord],
    transactions: &[TransactionRecord],
) {
    let rows = activity.iter().fold(Vec::new(), |mut rows, row| {
        if !rows
            .iter()
            .any(|other: &&ActivityRecord| equivalent(other, row))
        {
            rows.push(row);
        }
        rows
    });
    let closes: Vec<_> = rows
        .iter()
        .copied()
        .filter(|row| {
            activity_event(row)
                .is_some_and(|e| e.kind == EventKind::Close && e.id == exit.broker_trade_id)
        })
        .collect();
    let [closed] = closes.as_slice() else {
        return;
    };
    let opened: Vec<_> = rows
        .iter()
        .copied()
        .filter(|row| {
            activity_event(row)
                .is_some_and(|e| e.kind == EventKind::Open && e.id == exit.broker_trade_id)
        })
        .collect();
    if let [opened] = opened.as_slice() {
        exit.entry_price = opened.price;
        attach_cash(exit, opened, closed, &rows, transactions);
    }
    attach_order(exit, closed, &rows);
}

fn equivalent(a: &ActivityRecord, b: &ActivityRecord) -> bool {
    a.result == b.result
        && a.transaction_date == b.transaction_date
        && a.market == b.market
        && a.direction == b.direction
        && a.currency == b.currency
        && a.stake == b.stake
        && a.price == b.price
        && a.stop_order_price == b.stop_order_price
        && a.limit_order_price == b.limit_order_price
        && a.channel == b.channel
}

fn number(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok().filter(|n| n.is_finite())
}
fn price(a: Option<f64>, b: Option<f64>) -> bool {
    a.zip(b).is_some_and(|(a, b)| {
        a.is_finite() && b.is_finite() && a > 0.0 && b > 0.0 && (a - b).abs() < 1e-9
    })
}
fn near(
    a: Option<chrono::DateTime<chrono::FixedOffset>>,
    b: Option<chrono::DateTime<chrono::FixedOffset>>,
) -> bool {
    a.zip(b)
        .is_some_and(|(a, b)| (a - b).num_milliseconds().abs() <= 2000)
}
fn same_terms(a: &ActivityRecord, b: &ActivityRecord) -> bool {
    same_instrument(&a.market, &b.market)
        && a.currency == b.currency
        && a.direction == b.direction
        && number(&a.stake)
            .zip(number(&b.stake))
            .is_some_and(|(a, b)| a > 0.0 && (a - b).abs() < 1e-8)
}
fn cash_close(row: &TransactionRecord, closed: &ActivityRecord) -> bool {
    row.transaction_type == "2"
        && same_instrument(&row.description, &closed.market)
        && row.currency == closed.currency
        && !row.currency.is_empty()
        && !row.ref_id.is_empty()
        && near(row.transaction_date, closed.transaction_date)
        && price(number(&row.close_price), closed.price)
        && number(&row.amount)
            .zip(number(&closed.stake))
            .is_some_and(|(a, b)| b > 0.0 && (a.abs() - b).abs() < 1e-8)
}
fn cash_open(row: &TransactionRecord, opened: &ActivityRecord) -> bool {
    near(row.open_period, opened.transaction_date)
        && price(number(&row.open_price), opened.price)
        && number(&row.amount).is_some_and(|amount| match opened.direction.as_str() {
            "Sell" => amount < 0.0,
            "Buy" => amount > 0.0,
            _ => false,
        })
}
fn attach_cash(
    exit: &mut BrokerTradeExit,
    opened: &ActivityRecord,
    closed: &ActivityRecord,
    activity: &[&ActivityRecord],
    transactions: &[TransactionRecord],
) {
    let candidates: Vec<_> = transactions
        .iter()
        .filter(|row| cash_close(row, closed) && cash_open(row, opened))
        .collect();
    let [row] = candidates.as_slice() else {
        return;
    };
    // RefID is not a position ID. A second compatible close makes attribution ambiguous.
    if activity
        .iter()
        .filter(|other| {
            activity_event(other).is_some_and(|e| e.kind == EventKind::Close)
                && cash_close(row, other)
        })
        .count()
        != 1
    {
        return;
    }
    let Some(pl) = number(&row.profit_loss) else {
        return;
    };
    exit.realized_pl = Some(pl);
    // This is the settlement currency (CAD in the live fixture), not necessarily the account base currency.
    exit.account_currency = Some(row.currency.clone());
    exit.transaction_id = Some(row.ref_id.clone());
}
fn attach_order(exit: &mut BrokerTradeExit, closed: &ActivityRecord, activity: &[&ActivityRecord]) {
    let orders: Vec<_> = activity
        .iter()
        .copied()
        .filter(|row| {
            row.channel == "System"
                && same_terms(row, closed)
                && near(row.transaction_date, closed.transaction_date)
                && activity_event(row).is_some_and(|e| e.kind == EventKind::Execute)
        })
        .collect();
    let [order] = orders.as_slice() else {
        return;
    };
    if activity
        .iter()
        .filter(|row| {
            activity_event(row).is_some_and(|e| e.kind == EventKind::Close)
                && same_terms(order, row)
                && near(order.transaction_date, row.transaction_date)
        })
        .count()
        != 1
    {
        return;
    }
    let stop = price(order.stop_order_price, closed.price);
    let limit = price(order.limit_order_price, closed.price);
    let (reason, expected) = match (stop, limit) {
        (true, false) => (ExitReason::StopLoss, order.stop_order_price),
        (false, true) => (ExitReason::TakeProfit, order.limit_order_price),
        _ => return,
    };
    exit.reason = reason;
    exit.expected_price = expected;
    exit.exit_order_id = activity_event(order).map(|e| e.id);
}

#[cfg(test)]
mod tests;
