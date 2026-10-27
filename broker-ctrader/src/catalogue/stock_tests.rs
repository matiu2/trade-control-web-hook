use super::*;
use crate::market_data::AccountIdentity;
use instrument_lookup::AssetClass;

fn identity() -> AccountIdentity {
    tests::logging();
    AccountIdentity {
        account_id: 49247794,
        environment: ctrader_api::Environment::Demo,
        endpoint: ctrader_api::Environment::Demo.endpoint().into(),
        broker_name: "Spotware".into(),
        trader_login: 5944473,
    }
}

#[test]
fn all_native_stocks_keep_exact_identity_precision_and_declared_utc_clock() {
    tests::logging();
    assert_eq!(stock_symbols().unwrap().len(), 350);
    assert_eq!(non_stock_symbols().unwrap().len(), 93);
    for symbol in stock_symbols().unwrap() {
        let asset = stock_asset(symbol).unwrap();
        let metadata = stock_metadata(symbol).unwrap();
        assert_eq!(asset.id, symbol);
        assert_eq!(asset.class, AssetClass::Stock);
        assert_eq!(
            asset.news_currencies,
            std::slice::from_ref(&metadata.quote_asset.name)
        );
        assert_eq!(asset.decimal_places, metadata.digits as u8);
        assert_eq!(asset.pip_size, 10_f64.powi(-metadata.pip_position));
        assert_eq!(asset.tick_size, 10_f64.powi(-(metadata.digits as i32)));
        assert_eq!(asset.spread_schedule, "utc");
        assert_eq!(asset.spread_schedule_tz().as_deref(), Some("UTC"));
        assert!(asset.futures.is_none());
        assert!(metadata.holidays.is_empty());
        assert_eq!(
            verify_stock_metadata("ctrader-demo", &identity(), metadata).unwrap(),
            asset
        );
    }
}

#[test]
fn ambiguous_global_stock_tickers_never_supply_native_metadata() {
    tests::logging();
    let aia = stock_asset("AIA").unwrap();
    assert_eq!(aia.description, "AIA GROUP LTD");
    assert_eq!(aia.news_currencies, ["HKD"]);
    assert!(!aia.news_currencies.iter().any(|currency| currency == "AUD"));
    assert!(
        instrument_lookup::resolve("AIA")
            .unwrap()
            .is_none_or(|global| global != aia)
    );
    for name in ["AMP", "APA", "AIA"] {
        let asset = stock_asset(name).unwrap();
        assert_eq!(asset.id, name);
        assert!(asset.symbols.tradenation.is_none());
        assert!(
            replay_asset(name).is_err(),
            "accountless stock replay must remain gated"
        );
    }
    for alias in ["aia", "AIA_GROUP", "SINGAPOREAIRLINES", "NO_SUCH_SYMBOL"] {
        assert!(stock_asset(alias).is_err(), "{alias}");
    }
}

#[test]
fn stock_verification_rejects_any_changed_account_or_native_evidence() {
    let valid = stock_metadata("AIA").unwrap();
    for field in [
        "profile",
        "account",
        "environment",
        "broker",
        "endpoint",
        "login",
    ] {
        let mut current = identity();
        let mut profile = "ctrader-demo";
        match field {
            "profile" => profile = "other-demo",
            "account" => current.account_id += 1,
            "environment" => current.environment = ctrader_api::Environment::Live,
            "broker" => current.broker_name = "Other Broker".into(),
            "endpoint" => current.endpoint = "other-endpoint".into(),
            "login" => current.trader_login += 1,
            _ => unreachable!(),
        }
        assert!(
            verify_stock_metadata(profile, &current, valid).is_err(),
            "{field}"
        );
    }
    for field in [
        "id",
        "description",
        "currency",
        "digits",
        "pip",
        "session",
        "timezone",
        "class",
        "category",
        "lot",
        "mode",
        "enabled",
        "holiday",
    ] {
        let mut changed = valid.clone();
        match field {
            "id" => changed.id += 1,
            "description" => changed.description.push('!'),
            "currency" => changed.quote_asset.name = "USD".into(),
            "digits" => changed.digits += 1,
            "pip" => changed.pip_position += 1,
            "session" => changed.schedule[0].start_second += 1,
            "timezone" => changed.schedule_timezone = "Asia/Hong_Kong".into(),
            "class" => changed.asset_class.name = "US Shares".into(),
            "category" => changed.category.name = "Other Category".into(),
            "lot" => changed.lot_size += 1,
            "mode" => changed.trading_mode = 3,
            "enabled" => changed.enabled = false,
            "holiday" => changed.holidays.push(serde_json::json!({"holidayId": 1})),
            _ => unreachable!(),
        }
        assert!(
            verify_stock_metadata("ctrader-demo", &identity(), &changed).is_err(),
            "{field}"
        );
    }
}

#[test]
fn close_only_stock_is_readable_without_enabling_stock_orders() {
    let metadata = stock_metadata("VENTAS").unwrap();
    assert_eq!(metadata.trading_mode, 3);
    assert_eq!(metadata.id, 21991);
    assert!(verify_stock_metadata("ctrader-demo", &identity(), metadata).is_ok());
    assert!(replay_asset("VENTAS").is_err());
    let unusual = stock_metadata("REDES ENERGETICAS NACIONAIS").unwrap();
    assert_eq!(unusual.pip_position, 2);
    assert_eq!(stock_asset(&unusual.name).unwrap().pip_size, 0.01);
}

#[test]
fn native_metadata_decode_requires_matching_joins_and_required_session_precision() {
    let expected = stock_metadata("AIA").unwrap();
    let light = serde_json::json!({
        "symbolName": expected.name, "symbolId": expected.id,
        "description": expected.description, "enabled": expected.enabled,
        "baseAssetId": expected.base_asset.id, "quoteAssetId": expected.quote_asset.id,
        "symbolCategoryId": expected.category.id,
    });
    let full = serde_json::json!({
        "symbolId": expected.id, "digits": expected.digits, "pipPosition": expected.pip_position,
        "minVolume": expected.min_volume, "stepVolume": expected.step_volume,
        "maxVolume": expected.max_volume, "lotSize": expected.lot_size,
        "measurementUnits": expected.measurement_units, "tradingMode": expected.trading_mode,
        "scheduleTimeZone": expected.schedule_timezone,
        "schedule": expected.schedule.iter().map(|interval| serde_json::json!({
            "startSecond": interval.start_second, "endSecond": interval.end_second,
        })).collect::<Vec<_>>(),
    });
    let base = serde_json::json!({"assetId": expected.base_asset.id,
        "name": expected.base_asset.name, "displayName": expected.base_asset.display_name});
    let quote = serde_json::json!({"assetId": expected.quote_asset.id,
        "name": expected.quote_asset.name, "displayName": expected.quote_asset.display_name});
    let category = serde_json::json!({"id": expected.category.id,
        "name": expected.category.name, "assetClassId": expected.category.asset_class_id});
    let class = serde_json::to_value(&expected.asset_class).unwrap();
    let decode = |light: &serde_json::Value, full: &serde_json::Value| {
        StockMetadata::from_native(NativeStockMetadata {
            light,
            full_symbol: full,
            base_asset: &base,
            quote_asset: &quote,
            category: &category,
            asset_class: &class,
        })
    };
    assert_eq!(&decode(&light, &full).unwrap(), expected);
    for key in [
        "symbolId",
        "baseAssetId",
        "quoteAssetId",
        "symbolCategoryId",
    ] {
        let mut wrong = light.clone();
        wrong[key] = serde_json::json!(99999999);
        assert!(decode(&wrong, &full).is_err(), "{key}");
    }
    for key in [
        "digits",
        "pipPosition",
        "schedule",
        "scheduleTimeZone",
        "tradingMode",
        "lotSize",
    ] {
        let mut missing = full.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(decode(&light, &missing).is_err(), "{key}");
    }
}

#[test]
fn stock_inventory_keeps_all_native_classes_and_six_declared_session_groups() {
    tests::logging();
    let mut classes = std::collections::BTreeMap::new();
    let mut schedules = std::collections::BTreeSet::new();
    for symbol in stock_symbols().unwrap() {
        let metadata = stock_metadata(symbol).unwrap();
        *classes
            .entry(metadata.asset_class.name.as_str())
            .or_insert(0) += 1;
        schedules.insert(serde_json::to_string(&metadata.schedule).unwrap());
        assert_eq!(metadata.schedule_timezone, "UTC");
        assert!(metadata.canonical.is_none());
    }
    assert_eq!(
        classes,
        std::collections::BTreeMap::from([
            ("Asia/Pacific Shares", 183),
            ("EU Shares", 14),
            ("US Shares", 138),
            ("iShares ETFs", 15),
        ])
    );
    assert_eq!(schedules.len(), 6);
}
