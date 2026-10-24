use super::*;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn init_tracing() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn palladium() -> &'static instrument_lookup::Asset {
    instrument_lookup::resolve("XPDUSD")
        .expect("catalogue lookup")
        .expect("catalogued palladium")
}

fn native() -> metadata::NativeMetadata {
    metadata::NativeMetadata {
        name: "XPDUSD".into(),
        id: 107,
        digits: 3,
        pip_size: 0.01,
        min_units: 0.1,
        step_units: 0.1,
        max_units: 1000.0,
    }
}

#[test]
fn named_account_required_before_network() {
    init_tracing();
    for error in [resolve("EURUSD", None), resolve_replay("XPDUSD", Some(" "))] {
        assert!(
            error
                .unwrap_err()
                .to_string()
                .contains("explicit named account")
        );
    }
}

#[test]
fn trading_still_refuses_every_non_fx_class_before_connecting() {
    init_tracing();
    for symbol in ["XAUEUR", "AU200", "DE10YB", "XPDUSD", "BTCUSD", "3I"] {
        let error = resolve(symbol, Some("never-connect")).unwrap_err();
        assert!(
            error.to_string().contains("catalogued FX only"),
            "{symbol}: {error}"
        );
    }
}

#[test]
fn replay_class_policy_defers_metadata_readiness_to_audited_catalogue() {
    init_tracing();
    for class in [
        AssetClass::Forex,
        AssetClass::Index,
        AssetClass::Gold,
        AssetClass::Bond,
        AssetClass::Commodity,
        AssetClass::Crypto,
        AssetClass::Stock,
    ] {
        Mode::Replay
            .validate_class(class)
            .expect("supported replay class");
        assert_eq!(
            Mode::Trading.validate_class(class).is_ok(),
            class == AssetClass::Forex,
            "{class}"
        );
    }
}

#[test]
fn gold_eur_replay_uses_the_native_symbol_and_precision_without_contract_sizing() {
    init_tracing();
    let symbol = trade_control_conventions::instrument_for(
        trade_control_conventions::Broker::Ctrader,
        "XAU/EUR",
    );
    assert_eq!(symbol, "XAUEUR");
    let asset = Mode::Replay.asset(&symbol).unwrap();
    assert_eq!(asset.class, AssetClass::Gold);
    let metadata = metadata::NativeMetadata {
        name: "XAUEUR".into(),
        id: 22312,
        digits: 2,
        pip_size: 0.1,
        ..native()
    };
    let resolved = metadata::resolve(asset, &symbol, &metadata).unwrap();
    assert_eq!(resolved.broker_symbol, "XAUEUR");
    assert_eq!(resolved.precision.class, AssetClass::Gold);
    assert_eq!(resolved.precision.decimal_places, 2);
    assert_eq!(resolved.precision.tick_size, 0.01);
    assert_eq!(resolved.precision.pip_size, 0.1);
    assert!(resolved.precision.contract_multiplier.is_none());
}

#[test]
fn missing_catalogue_metadata_still_fails_before_connecting() {
    init_tracing();
    let error =
        resolve_replay("NONEXISTENT_CTRADER_TEST_SYMBOL", Some("never-connect")).unwrap_err();
    assert!(error.to_string().contains("not ready for replay metadata"));
}

#[test]
fn palladium_replay_uses_native_precision_without_inventing_contract_sizing() {
    init_tracing();
    let resolved = metadata::resolve(palladium(), "XPDUSD", &native()).expect("native metadata");
    assert_eq!(resolved.broker_symbol, "XPDUSD");
    assert_eq!(resolved.precision.class, AssetClass::Commodity);
    assert_eq!(resolved.precision.decimal_places, 3);
    assert_eq!(resolved.precision.tick_size, 0.001);
    assert_eq!(resolved.precision.pip_size, 0.01);
    assert!(resolved.precision.contract_multiplier.is_none());
}

#[test]
fn forex_precision_preserves_the_brokers_pip_convention() {
    init_tracing();
    let asset = instrument_lookup::resolve("EURCZK")
        .expect("catalogue lookup")
        .expect("catalogued forex");
    let native = metadata::NativeMetadata {
        name: "EURCZK".into(),
        digits: 4,
        pip_size: 0.001,
        ..native()
    };
    let resolved = metadata::resolve(asset, "EURCZK", &native).expect("broker convention");
    assert_eq!(resolved.precision.pip_size, 0.001);
    assert_ne!(resolved.precision.pip_size, asset.pip_size);
    assert_eq!(resolved.precision.tick_size, 0.0001);
    assert_eq!(resolved.precision.class, AssetClass::Forex);
    assert!(resolved.precision.contract_multiplier.is_none());
}

#[test]
fn wrong_symbol_identity_and_precision_are_refused() {
    init_tracing();
    for row in [
        metadata::NativeMetadata {
            name: "XPD_USD".into(),
            ..native()
        },
        metadata::NativeMetadata { id: 0, ..native() },
        metadata::NativeMetadata { id: -1, ..native() },
        metadata::NativeMetadata {
            digits: 9,
            ..native()
        },
        metadata::NativeMetadata {
            digits: u32::MAX,
            ..native()
        },
    ] {
        assert!(metadata::resolve(palladium(), "XPDUSD", &row).is_err());
    }
}

#[test]
fn invalid_pip_and_volume_metadata_are_refused() {
    init_tracing();
    for invalid in [0.0, -0.1, f64::NAN, f64::INFINITY] {
        for row in [
            metadata::NativeMetadata {
                pip_size: invalid,
                ..native()
            },
            metadata::NativeMetadata {
                min_units: invalid,
                ..native()
            },
            metadata::NativeMetadata {
                step_units: invalid,
                ..native()
            },
            metadata::NativeMetadata {
                max_units: invalid,
                ..native()
            },
        ] {
            assert!(metadata::resolve(palladium(), "XPDUSD", &row).is_err());
        }
    }
    for row in [
        metadata::NativeMetadata {
            max_units: 0.01,
            ..native()
        },
        metadata::NativeMetadata {
            step_units: 1001.0,
            ..native()
        },
    ] {
        assert!(metadata::resolve(palladium(), "XPDUSD", &row).is_err());
    }
}

#[test]
fn replay_uses_audited_native_nonstock_underlyings() {
    init_tracing();
    let nok_jpy = Mode::Replay.asset("NOKJPY").unwrap();
    assert_eq!(nok_jpy.class, AssetClass::Forex);
    assert_eq!(nok_jpy.pip_size, 0.0001);
    let bitcoin_eur = Mode::Replay.asset("BTCEUR").unwrap();
    assert_eq!(bitcoin_eur.class, AssetClass::Crypto);
    assert_eq!(bitcoin_eur.news_currencies, ["BTC", "EUR"]);
    let dow = Mode::Replay.asset("US 30").unwrap();
    assert_eq!(dow.id, "US30");
    assert_eq!(dow.class, AssetClass::Index);
    let native = metadata::NativeMetadata {
        name: "US 30".into(),
        id: 100,
        digits: 2,
        pip_size: 0.1,
        ..native()
    };
    let resolved = metadata::resolve(dow, "US 30", &native).unwrap();
    assert_eq!(resolved.broker_symbol, "US 30");
    assert!(resolved.precision.contract_multiplier.is_none());
}

#[test]
fn unaudited_stocks_and_aliases_fail_before_connecting() {
    init_tracing();
    for symbol in ["MSFT", "3I", "AMP", "APA", "US30"] {
        let error = resolve_replay(symbol, Some("never-connect")).unwrap_err();
        assert!(
            error.to_string().contains("not ready for replay metadata"),
            "{symbol}: {error}"
        );
    }
}

#[test]
fn replay_underlying_rollout_does_not_expand_live_forex_metadata() {
    init_tracing();
    assert!(Mode::Replay.asset("NOKJPY").is_ok());
    assert!(Mode::Trading.asset("NOKJPY").is_err());
}
