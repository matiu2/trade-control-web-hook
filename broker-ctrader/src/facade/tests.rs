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
        setup_id: None,
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

/// Explicitly opted-in, read-only integration check against a configured demo.
#[tokio::test]
#[ignore = "requires CTRADER_METADATA_TEST_ACCOUNT and Open API demo access"]
async fn hybrid_symbol_validation_succeeds_with_fix_offline() {
    init_tracing();
    let account = std::env::var("CTRADER_METADATA_TEST_ACCOUNT").unwrap();
    let access::Profile::Hybrid {
        open_api,
        mut quote,
        trade,
        binding,
    } = access::Profile::load(&account).unwrap()
    else {
        panic!("integration check requires a hybrid demo profile");
    };
    assert_eq!(open_api.environment, ctrader_api::Environment::Demo);
    let offline = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    quote.host = "127.0.0.1".into();
    quote.port = offline.local_addr().unwrap().port();
    drop(offline);
    let profile = access::Profile::Hybrid {
        open_api,
        quote,
        trade,
        binding,
    };
    let broker = CtraderBroker::connect_profile(&account, profile)
        .await
        .unwrap();
    assert_eq!(
        broker.canonical_instrument("USD_JPY").await.unwrap(),
        "USDJPY"
    );
    assert!(
        broker
            .canonical_instrument("NO_SUCH_INSTRUMENT")
            .await
            .is_err()
    );
}

/// Exercises real account sizing and quote reads; the request cannot submit an order.
#[tokio::test]
#[ignore = "requires CTRADER_METADATA_TEST_ACCOUNT and Open API demo access"]
async fn hybrid_market_dry_run_validates_native_eurgbp_without_submission() {
    init_tracing();
    let account = std::env::var("CTRADER_METADATA_TEST_ACCOUNT").unwrap();
    let broker = CtraderBroker::connect(&account).await.unwrap();
    let Backend::Hybrid(hybrid) = &broker.backend else {
        panic!("integration check requires a hybrid demo profile");
    };
    let symbol = broker.instrument_metadata("EURGBP").await.unwrap();
    let quote = crate::metadata::quote(&hybrid.data, &symbol.name)
        .await
        .unwrap();
    let scale = 10_f64.powi(symbol.digits as i32);
    let snap = |price: f64| (price * scale).round() / scale;
    let request = EntryRequest {
        setup_id: None,
        instrument: &symbol.name,
        direction: Direction::Short,
        entry: ResolvedEntry::Market {
            reference_price: quote.bid,
        },
        stop_loss: snap(quote.ask + 10.0 * symbol.pip_size),
        take_profit: snap(quote.bid - 20.0 * symbol.pip_size),
        risk: RiskBudget::Units(symbol.min_units),
        dry_run: true,
        contract_multiplier: None,
    };
    let placement = broker.place_entry(1.0, u32::MAX, &request).await.unwrap();
    assert_eq!(placement.order_id, "dry-run");
    assert_eq!(placement.size, Some(symbol.min_units));
    assert!(placement.price.is_some_and(|price| price > 0.0));
}
