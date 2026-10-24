use super::*;
use tracing_subscriber::prelude::*;

fn init_tracing() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn plan(symbol: &str, brokers: &[&str]) -> TradePlan {
    serde_json::from_value(serde_json::json!({
        "trade_id":"sentiment", "instrument":symbol, "direction":"long",
        "granularity":"h1", "pip_size":0.0001,
        "rules":brokers.iter().enumerate().map(|(index, broker)| serde_json::json!({
            "rule_id":index.to_string(), "trigger":{"type":"time_reached", "at_epoch":1},
            "fire_mode":"once", "intent":{
                "v":1, "id":index.to_string(), "not_after":"2027-01-01T00:00:00Z",
                "action":"veto", "instrument":symbol, "broker":broker, "account":"demo"
            }
        })).collect::<Vec<_>>()
    }))
    .unwrap()
}

#[test]
fn native_ctrader_sentiment_uses_verified_currency_legs() {
    init_tracing();
    for (symbol, expected) in [
        ("NOKJPY", vec!["NOK", "JPY"]),
        ("BTCEUR", vec!["BTC", "EUR"]),
        ("ITALY 40", vec!["EUR"]),
        ("US 30", vec!["USD"]),
    ] {
        let asset = asset_for_plan(&plan(symbol, &["ctrader"])).unwrap();
        assert_eq!(asset.news_currencies, expected, "{symbol}");
    }
    assert_eq!(
        asset_for_plan(&plan("NAT.GAS", &["ctrader"])).unwrap().id,
        "NATGASUSD"
    );
}

#[test]
fn mixed_or_unaudited_ctrader_metadata_is_not_guessed() {
    init_tracing();
    assert!(asset_for_plan(&plan("EURUSD", &["ctrader", "oanda"])).is_err());
    assert!(asset_for_plan(&plan("AMP", &["ctrader"])).is_err());
}

#[test]
fn legacy_sentiment_keeps_global_catalogue_resolution() {
    init_tracing();
    for brokers in [vec![], vec!["oanda"], vec!["tradenation"]] {
        assert_eq!(
            asset_for_plan(&plan("EURUSD", &brokers)).unwrap().id,
            "EURUSD"
        );
    }
}
