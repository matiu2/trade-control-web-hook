use super::*;
use ctrader_fix::trading::{OrderStatus, Side};
use tracing_subscriber::prelude::*;

pub(super) fn init() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
pub(super) fn configs() -> (Config, TradeConfig) {
    init();
    let quote = Config {
        host: "quote.example.com".into(),
        port: 5201,
        sender_comp_id: "demo.broker.123".into(),
        target_comp_id: "cServer".into(),
        sender_sub_id: "QUOTE".into(),
        password: "quote-secret".into(),
    };
    let trade = TradeConfig {
        host: "trade.example.com".into(),
        port: 5202,
        sender_comp_id: quote.sender_comp_id.clone(),
        target_comp_id: "cServer".into(),
        sender_sub_id: "TRADE".into(),
        password: "trade-secret".into(),
    };
    (quote, trade)
}
fn symbol(id: u64, name: &str) -> Symbol {
    Symbol {
        id,
        name: name.into(),
        digits: 5,
    }
}
fn position() -> Position {
    Position {
        position_id: "7".into(),
        symbol_id: 42,
        long_quantity: 1000.0,
        short_quantity: 0.0,
        average_price: 1.1,
        stop_loss: Some(1.09),
        take_profit: Some(1.12),
    }
}
fn report() -> ExecutionReport {
    ExecutionReport {
        order_id: "9".into(),
        client_order_id: Some("entry-9".into()),
        original_client_order_id: None,
        status: OrderStatus::New,
        exec_type: "0".into(),
        symbol_id: Some(42),
        side: Some(Side::Buy),
        quantity: Some(1000.0),
        cumulative_quantity: Some(0.0),
        leaves_quantity: Some(1000.0),
        average_price: None,
        last_quantity: None,
        position_id: None,
        limit_price: Some(1.0),
        stop_price: None,
        stop_loss: None,
        take_profit: None,
    }
}
#[test]
fn credentials_pin_the_same_demo_account_without_conflating_endpoints() -> Result<()> {
    let (q, mut t) = configs();
    validate_pair(&q, &t)?;
    t.sender_comp_id = "demo.broker.124".into();
    assert!(validate_pair(&q, &t).is_err());
    t.sender_comp_id = q.sender_comp_id.replace("demo.", "live.");
    assert!(validate_pair(&q, &t).is_err());
    t.sender_comp_id = q.sender_comp_id.clone();
    t.target_comp_id = "CSERVER".into();
    assert!(validate_pair(&q, &t).is_err());
    Ok(())
}
#[test]
fn account_gate_is_shared_across_aliases_and_rotated_credentials() -> Result<()> {
    let (q, mut t) = configs();
    let first = account_gate(&q)?;
    t.password = "rotated".into();
    let mut other = q.clone();
    other.password = t.password;
    other.host = "another.example.com".into();
    assert!(Arc::ptr_eq(&first, &account_gate(&other)?));
    Ok(())
}
#[test]
fn symbols_preserve_native_names_and_reject_ambiguity() -> Result<()> {
    init();
    let symbols = vec![symbol(42, "EURUSD"), symbol(43, "EUR_USD")];
    assert_eq!(mapping::resolve(&symbols, "EURUSD")?.id, 42);
    assert!(mapping::resolve(&symbols, "EUR/USD").is_err());
    assert!(mapping::resolve(&[symbol(44, "EURUSD.c")], "EURUSD").is_err());
    assert_eq!(mapping::resolve(&[symbol(42, "EURUSD")], "EUR_USD")?.id, 42);
    Ok(())
}
#[test]
fn position_mapping_has_real_protection_and_no_fabricated_origin() -> Result<()> {
    init();
    let mapped = mapping::position(&[symbol(42, "EURUSD")], &position())?;
    assert_eq!(mapped.order_id, "");
    assert_eq!(mapped.stake, 1000.0);
    assert_eq!(mapped.stop_loss, Some(1.09));
    assert_eq!(mapped.take_profit, Some(1.12));
    assert_eq!(mapped.opened_at, None);
    Ok(())
}
#[test]
fn invalid_or_two_sided_positions_cannot_generate_close_orders() {
    init();
    let mut p = position();
    p.long_quantity = 0.0;
    assert!(mapping::close_order(&p, "close-1".into()).is_err());
    p.long_quantity = 1000.0;
    p.short_quantity = 1000.0;
    assert!(mapping::close_order(&p, "close-1".into()).is_err());
    p.short_quantity = 0.0;
    p.position_id.clear();
    assert!(mapping::close_order(&p, "close-1".into()).is_err());
}
#[test]
fn partial_close_does_not_confirm_flat() {
    init();
    let mut r = report();
    r.status = OrderStatus::PartiallyFilled;
    r.exec_type = "F".into();
    r.cumulative_quantity = Some(500.0);
    r.leaves_quantity = Some(500.0);
    r.position_id = Some("7".into());
    let p = position();
    assert!(mapping::confirmed_close(&p, &r, &[]).is_err());
    r.status = OrderStatus::Filled;
    r.cumulative_quantity = Some(1000.0);
    r.leaves_quantity = Some(0.0);
    r.side = Some(Side::Sell);
    r.average_price = Some(1.101);
    assert!(mapping::confirmed_close(&p, &r, std::slice::from_ref(&p)).is_err());
    assert!(mapping::confirmed_close(&p, &r, &[]).is_ok());
}
#[test]
fn pending_lifecycle_and_linked_attempt_evidence() -> Result<()> {
    init();
    let r = report();
    let pending = mapping::pending(&[symbol(42, "EURUSD")], &r)?;
    assert_eq!(pending.trigger, 1.0);
    assert!(!pending.is_stop);
    assert_eq!(
        mapping::attempt(42, "9", None, std::slice::from_ref(&r), &[])?,
        AttemptState::Pending
    );
    assert_eq!(
        mapping::attempt(42, "missing", None, &[r], &[])?,
        AttemptState::Unknown
    );
    assert_eq!(
        mapping::attempt(42, "9", Some("7"), &[], &[position()])?,
        AttemptState::OpenPosition {
            broker_trade_id: "7".into()
        }
    );
    assert_eq!(
        mapping::attempt(42, "9", Some("7"), &[], &[])?,
        AttemptState::Unknown
    );
    Ok(())
}
#[test]
fn contradictory_attempt_instrument_does_not_match() {
    init();
    assert!(mapping::attempt(41, "9", None, &[report()], &[]).is_err());
    assert!(mapping::attempt(41, "9", Some("7"), &[], &[position()]).is_err());
}
