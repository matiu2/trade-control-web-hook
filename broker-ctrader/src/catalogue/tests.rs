use super::*;
use instrument_lookup::AssetClass;
use tracing_subscriber::prelude::*;

pub(super) fn logging() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

#[test]
fn every_verified_native_nonstock_has_one_underlying() {
    logging();
    let symbols = non_stock_symbols().unwrap();
    assert_eq!(symbols.len(), 93);
    for symbol in symbols {
        let asset = replay_asset(symbol).unwrap();
        assert_ne!(asset.class, AssetClass::Stock, "{symbol}");
        assert!(asset.spread_schedule_tz().is_some(), "{symbol}");
        assert!(asset.futures.is_none(), "{symbol}");
    }
}

#[test]
fn existing_forex_and_palladium_calibration_clocks_are_unchanged() {
    logging();
    let snapshot = facts::snapshot().unwrap();
    for record in snapshot.records.iter().filter(|r| r.canonical.is_some()) {
        let original = instrument_lookup::by_id(record.canonical.as_deref().unwrap())
            .unwrap()
            .unwrap();
        let mapped = replay_asset(&record.symbol).unwrap();
        assert_eq!(mapped, original, "{}", record.symbol);
    }
}

#[test]
fn native_fx_news_and_unusual_jpy_precision_follow_verified_metadata() {
    logging();
    let asset = replay_asset("NOKJPY").unwrap();
    assert_eq!(asset.class, AssetClass::Forex);
    assert_eq!(asset.news_currencies, ["NOK", "JPY"]);
    assert_eq!(asset.decimal_places, 5);
    assert_eq!(asset.tick_size, 0.00001);
    assert_eq!(asset.pip_size, 0.0001);
    assert_eq!(asset.spread_schedule, "ny");
    assert!(asset.symbols.oanda.is_none());
    assert!(asset.symbols.tradenation.is_none());
}

#[test]
fn crypto_quote_currency_and_learning_clock_do_not_become_usd_or_broker_session_time() {
    logging();
    for (symbol, base) in [("BTCEUR", "BTC"), ("ETHEUR", "ETH")] {
        let asset = replay_asset(symbol).unwrap();
        assert_eq!(asset.class, AssetClass::Crypto);
        assert_eq!(asset.news_currencies, [base, "EUR"]);
        assert_eq!(asset.spread_schedule, "ny");
        assert!(asset.futures.is_none());
    }
}

#[test]
fn vetted_aliases_keep_native_names_separate_from_underlying_metadata() {
    logging();
    for (native, canonical) in [
        ("US 30", "US30"),
        ("XBRUSD", "BCO"),
        ("XTIUSD", "WTICOUSD"),
        ("NAT.GAS", "NATGASUSD"),
    ] {
        assert_eq!(replay_asset(native).unwrap().id, canonical);
    }
    let italy = replay_asset("ITALY 40").unwrap();
    assert_eq!(italy.id, "ITALY40");
    assert_eq!(italy.class, AssetClass::Index);
    assert_eq!(italy.news_currencies, ["EUR"]);
    assert_eq!(italy.spread_schedule_tz().as_deref(), Some("Europe/Berlin"));
}

#[test]
fn stock_alias_hazards_and_unknown_symbols_never_fall_through_to_global_lookup() {
    logging();
    for symbol in ["AMP", "APA", "MSFT", "NO_SUCH_SYMBOL", "", "XPD_USD"] {
        assert!(replay_asset(symbol).is_err(), "{symbol}");
    }
}

#[test]
fn snapshot_requires_real_unique_native_metadata() {
    logging();
    let valid = facts::snapshot().unwrap();
    facts::validate(&valid).unwrap();
    let mut duplicate = valid.clone();
    duplicate.records.push(duplicate.records[0].clone());
    assert!(facts::validate(&duplicate).is_err());
    for invalid in [
        "source", "id", "class", "currency", "digits", "pip", "volume", "timezone",
    ] {
        let mut changed = valid.clone();
        let record = changed
            .records
            .iter_mut()
            .find(|r| r.symbol == "NOKJPY")
            .unwrap();
        match invalid {
            "source" => changed.source_account_id = 0,
            "id" => record.source_id = 0,
            "class" => record.source_class = "US Shares".into(),
            "currency" => record.quote_asset = "XAU".into(),
            "digits" => record.digits = 99,
            "pip" => record.pip_position = -1,
            "volume" => record.step_volume = 0,
            "timezone" => record.native_timezone.clear(),
            _ => unreachable!(),
        }
        assert!(facts::validate(&changed).is_err(), "{invalid}");
    }
}
