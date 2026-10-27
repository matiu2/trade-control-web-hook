use super::*;
use tracing_subscriber::prelude::*;

fn init() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
fn source() -> (Value, Value) {
    init();
    (
        json!({"ctidTraderAccountId":7,"order":{"orderId":10,"clientOrderId":"probe-1","positionId":20,"orderStatus":2,"executedVolume":100000,"closingOrder":false,"tradeData":{"symbolId":8,"tradeSide":1,"volume":100000}}}),
        json!({"ctidTraderAccountId":7,"order":[],"position":[{"positionId":20,"positionStatus":1,"tradeData":{"symbolId":8,"tradeSide":1,"volume":100000}}]}),
    )
}
fn closure() -> Value {
    json!({"ctidTraderAccountId":7,"order":{"orderId":11,"positionId":20,"orderStatus":2,"executedVolume":100000,"closingOrder":true,"tradeData":{"symbolId":8,"tradeSide":2}},"deal":[{"dealId":30,"orderId":11,"positionId":20,"symbolId":8,"tradeSide":2,"dealStatus":2,"filledVolume":100000,"closePositionDetail":{"closedVolume":100000}}]})
}
#[test]
fn exact_position_and_full_closing_deals_are_required() {
    let (d, s) = source();
    let t = opening(&d, &s, 7, 10, 20, "probe-1").unwrap();
    assert!(closed(&closure(), 7, 11, 20, &t).unwrap());
    let empty = json!({"ctidTraderAccountId":7,"position":[],"order":[]});
    absent(&empty, 7, 20).unwrap();
    assert!(absent(&s, 7, 20).is_err());
}
#[test]
fn unrelated_order_position_account_and_partial_closure_refuse() {
    let (d, s) = source();
    assert!(opening(&d, &s, 7, 99, 20, "probe-1").is_err());
    assert!(opening(&d, &s, 7, 10, 99, "probe-1").is_err());
    assert!(opening(&d, &s, 99, 10, 20, "probe-1").is_err());
    assert!(opening(&d, &s, 7, 10, 20, "somebody-else").is_err());
    let t = opening(&d, &s, 7, 10, 20, "probe-1").unwrap();
    for mode in 0..4 {
        let mut c = closure();
        match mode {
            0 => c["order"]["positionId"] = json!(99),
            1 => c["order"]["executedVolume"] = json!(50000),
            2 => c["deal"][0]["closePositionDetail"]["closedVolume"] = json!(50000),
            _ => c["deal"][0]["orderId"] = json!(99),
        }
        assert!(closed(&c, 7, 11, 20, &t).is_err());
    }
}
#[test]
fn acceptance_is_not_closure_and_duplicate_deals_refuse() {
    let (d, s) = source();
    let t = opening(&d, &s, 7, 10, 20, "probe-1").unwrap();
    let mut c = closure();
    c["order"]["orderStatus"] = json!(1);
    assert!(!closed(&c, 7, 11, 20, &t).unwrap());
    c = closure();
    let duplicate = c["deal"][0].clone();
    c["deal"].as_array_mut().unwrap().push(duplicate);
    assert!(closed(&c, 7, 11, 20, &t).is_err());
}
