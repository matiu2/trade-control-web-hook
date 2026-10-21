use super::*;
use trade_control_core::intent::{Direction, ResolvedEntry, RiskBudget};

fn profile() -> access::Profile {
    let quote = ctrader_fix::Config {
        host: "unresolvable.invalid".into(),
        port: 5211,
        sender_comp_id: "demo.example.123".into(),
        target_comp_id: "cServer".into(),
        sender_sub_id: "QUOTE".into(),
        password: "test-only".into(),
    };
    let trade = ctrader_fix::TradeConfig {
        host: quote.host.clone(),
        port: 5212,
        sender_comp_id: quote.sender_comp_id.clone(),
        target_comp_id: quote.target_comp_id.clone(),
        sender_sub_id: "TRADE".into(),
        password: quote.password.clone(),
    };
    access::Profile::FixTrading { quote, trade }
}

fn init_tracing() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

#[tokio::test]
async fn fix_has_no_open_api_account_id_or_fabricated_risk_metadata() {
    init_tracing();
    let broker = CtraderBroker::connect_profile("demo", profile())
        .await
        .unwrap();
    assert_eq!(
        broker.account_id(),
        None,
        "terminal login is not a ctidTraderAccountId"
    );
    assert!(
        broker
            .equity()
            .await
            .unwrap_err()
            .to_string()
            .contains("equity")
    );
    assert!(
        broker
            .instrument_metadata("EURUSD")
            .await
            .unwrap_err()
            .to_string()
            .contains("volume limits")
    );
}

#[tokio::test]
async fn explicit_units_do_not_bypass_required_entry_protection_or_risk_data() {
    init_tracing();
    let broker = CtraderBroker::connect_profile("demo", profile())
        .await
        .unwrap();
    let request = EntryRequest {
        instrument: "EURUSD",
        direction: Direction::Long,
        entry: ResolvedEntry::Stop { trigger_price: 1.1 },
        stop_loss: 1.09,
        take_profit: 1.12,
        risk: RiskBudget::Units(1000.0),
        dry_run: false,
        contract_multiplier: None,
    };
    let error = broker.place_entry(1.0, 3, &request).await.unwrap_err();
    assert!(matches!(error, EntryError::OrderRejectedWithDetail(_)));
    assert!(error.to_string().contains("Open API"));
}

#[tokio::test]
async fn quote_only_profile_cannot_become_a_management_connection() {
    init_tracing();
    let access::Profile::FixTrading { quote, .. } = profile() else {
        panic!("fixture");
    };
    let result = CtraderBroker::connect_profile("demo", access::Profile::Fix(quote)).await;
    assert!(result.is_err());
}
