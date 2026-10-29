use super::*;
use trade_control_core::{
    attempt_resolution::{self, Evidence},
    broker::AttemptState,
};

fn fixture() -> (BrokerTradeExit, Vec<ActivityRecord>, Vec<TransactionRecord>) {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let activity: Vec<ActivityRecord> = serde_json::from_str(include_str!(
        "../../tests/fixtures/audcad-2026-10-29-activity.json"
    ))
    .unwrap();
    let transactions = serde_json::from_str(include_str!(
        "../../tests/fixtures/audcad-2026-10-29-transactions.json"
    ))
    .unwrap();
    let evidence = Evidence {
        events: activity
            .iter()
            .filter_map(crate::attempts::activity_event)
            .collect(),
        history_complete: true,
        ..Evidence::default()
    };
    let AttemptState::ClosedConfirmed { exit } =
        attempt_resolution::resolve("96523425", Some("95397401"), &evidence).state
    else {
        panic!("fixture must close")
    };
    (*exit, activity, transactions)
}

#[test]
fn live_audcad_exit_has_cash_profit_and_executed_take_profit() {
    let (mut exit, activity, transactions) = fixture();
    enrich(&mut exit, &activity, &transactions);
    assert_eq!(exit.realized_pl, Some(12.55));
    assert_eq!(exit.account_currency.as_deref(), Some("CAD"));
    assert_eq!(exit.transaction_id.as_deref(), Some("188187546"));
    assert_eq!(exit.entry_price, Some(0.98564));
    assert_eq!(exit.exit_price, Some(0.98407));
    assert_eq!(exit.reason, ExitReason::TakeProfit);
    assert_eq!(exit.expected_price, Some(0.98407));
    assert_eq!(exit.exit_order_id.as_deref(), Some("96523480"));
}

#[test]
fn missing_or_ambiguous_cash_evidence_stays_unknown() {
    let (original, activity, transactions) = fixture();
    for rows in [
        vec![],
        vec![transactions[0].clone(), {
            let mut t = transactions[0].clone();
            t.ref_id = "another".into();
            t
        }],
    ] {
        let mut exit = original.clone();
        enrich(&mut exit, &activity, &rows);
        assert_eq!(exit.realized_pl, None);
        assert_eq!(exit.transaction_id, None);
        assert_eq!(
            exit.reason,
            ExitReason::TakeProfit,
            "cash delays do not hide executed order evidence"
        );
    }
}

#[test]
fn unrelated_or_malformed_transactions_do_not_supply_profit() {
    let (original, activity, transactions) = fixture();
    for field in [
        "market",
        "open_time",
        "close_time",
        "price",
        "size",
        "currency",
        "funding",
        "nan",
    ] {
        let mut row = transactions[0].clone();
        match field {
            "market" => row.description = "CHF/JPY".into(),
            "open_time" => {
                row.open_period = row.open_period.map(|t| t + chrono::Duration::minutes(1))
            }
            "close_time" => {
                row.transaction_date = row
                    .transaction_date
                    .map(|t| t + chrono::Duration::minutes(1))
            }
            "price" => row.open_price = "0.98565".into(),
            "size" => row.amount = "-0.16".into(),
            "currency" => row.currency = "AUD".into(),
            "funding" => row.transaction_type = "3".into(),
            _ => row.profit_loss = "NaN".into(),
        }
        let mut exit = original.clone();
        enrich(&mut exit, &activity, &[row]);
        assert_eq!(exit.realized_pl, None, "{field}");
    }
}

#[test]
fn profitable_stop_and_manual_close_are_not_called_take_profit() {
    let (original, mut activity, transactions) = fixture();
    let row = activity
        .iter_mut()
        .find(|r| r.result == "Execute Order:96523480")
        .unwrap();
    row.stop_order_price = Some(0.98407);
    row.limit_order_price = Some(0.983);
    let mut exit = original.clone();
    enrich(&mut exit, &activity, &transactions);
    assert_eq!(exit.reason, ExitReason::StopLoss);
    assert_eq!(exit.realized_pl, Some(12.55));
    activity.retain(|r| r.result != "Execute Order:96523480");
    let mut exit = original;
    enrich(&mut exit, &activity, &transactions);
    assert_eq!(exit.reason, ExitReason::Unknown);
    assert_eq!(exit.realized_pl, Some(12.55));
}

#[test]
fn competing_positions_and_exit_orders_do_not_share_evidence() {
    let (original, mut activity, transactions) = fixture();
    let mut other = activity
        .iter()
        .find(|r| r.result == "Close Position:95397401")
        .unwrap()
        .clone();
    other.result = "Close Position:other".into();
    activity.push(other);
    let mut exit = original;
    enrich(&mut exit, &activity, &transactions);
    assert_eq!(exit.realized_pl, None);
    assert_eq!(exit.reason, ExitReason::Unknown);
}
