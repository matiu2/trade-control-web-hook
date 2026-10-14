use super::*;
use instrument_lookup::AssetClass;
use mt5_data_source::web::trading::Rules;

fn logging() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn asset(raw: &str) -> &'static Asset {
    instrument_lookup::resolve(&catalog_symbol(raw))
        .expect("catalog loads")
        .expect("known underlying")
}

fn listing(names: &[&str]) -> Vec<Symbol> {
    names
        .iter()
        .map(|name| Symbol {
            name: (*name).into(),
            id: 1,
            digits: 3,
        })
        .collect()
}

fn metadata(name: &str, digits: u32, tick_size: f64, mode: u32) -> SymbolInfo {
    SymbolInfo {
        name: name.into(),
        digits,
        point: 10_f64.powi(-(digits as i32)),
        tick_size,
        calc_mode: mode,
        contract_size: 5_000.0,
        base_currency: "XAG".into(),
        profit_currency: "USD".into(),
        rules: Rules {
            path: String::new(),
            spread_points: 0,
            spread_balance: 0,
            trade_mode: 4,
            stops_points: 0,
            execution: 2,
            filling_flags: 2,
            expiration_flags: 1,
            order_flags: 127,
            request_timeout_seconds: 7,
            min_volume_raw: 1_000_000,
            max_volume_raw: 10_000_000_000,
            volume_step_raw: 1_000_000,
        },
    }
}

#[test]
fn silver_uses_mt5_precision_and_keeps_news_currencies() {
    logging();
    let silver = asset("XAGUSD");
    let resolved = from_metadata(silver, &metadata("XAGUSD", 3, 0.001, 2)).expect("silver CFD");
    assert_eq!(resolved.broker_symbol, "XAGUSD");
    assert_eq!(resolved.precision.class, AssetClass::Commodity);
    assert_eq!(resolved.precision.tick_size, 0.001);
    assert_eq!(resolved.precision.pip_size, 0.001);
    assert_eq!(resolved.precision.decimal_places, 3);
    assert_eq!(resolved.precision.contract_multiplier, None);
    assert_ne!(silver.tick_size, resolved.precision.tick_size);
    assert!(resolved.asset.is_affected_by("USD"));
    let key = trade_control_core::spread_blackout::mt5_spread_key(
        "the5ers-competition",
        &resolved.broker_symbol,
    );
    assert!(trade_control_core::spread_blackout::coverage(&key).is_covered());
}

#[test]
fn native_aliases_resolve_indices_and_oil_to_shared_underlyings() {
    logging();
    [
        ("SP500", "SPX500"),
        ("DAX40", "DE30"),
        ("JPN225", "JP225"),
        ("XBRUSD", "BCO"),
        ("XTIUSD", "WTICO"),
        ("NAS100", "NAS100"),
        ("XAUUSD", "XAUUSD"),
        ("BTCUSD", "BTCUSD"),
        ("ETHUSD", "ETHUSD"),
    ]
    .into_iter()
    .for_each(|(native, canonical)| {
        let asset = asset(native);
        assert_eq!(asset.id, canonical);
        assert_eq!(
            select_symbol(native, asset, &listing(&[native])).unwrap(),
            native
        );
    });
    assert_eq!(
        select_symbol("SPX500", asset("SPX500"), &listing(&["SP500"])).unwrap(),
        "SP500"
    );
    assert_eq!(
        select_symbol("Spot Silver", asset("XAGUSD"), &listing(&["XAGUSD"])).unwrap(),
        "XAGUSD"
    );
}

#[test]
fn duplicate_catalog_rows_for_other_brokers_select_the_same_mt5_underlying() {
    logging();
    [
        ("SPX500USD", "SP500"),
        ("DE30EUR", "DAX40"),
        ("WTICOUSD", "XTIUSD"),
        ("BCOUSD", "XBRUSD"),
    ]
    .into_iter()
    .for_each(|(alias, native)| {
        assert_eq!(
            select_symbol(
                alias,
                instrument_lookup::resolve(alias).unwrap().unwrap(),
                &listing(&[native])
            )
            .unwrap(),
            native
        );
    });
    assert!(!same_underlying(asset("JP225"), asset("JP225USD")));
    assert!(select_symbol("JP225USD", asset("JP225USD"), &listing(&["JPN225"])).is_err());
    let mut oil = asset("WTICO").clone();
    oil.symbols.oanda = None;
    let mut usd = oil.clone();
    usd.id = "WTICOUSD".into();
    usd.symbols.oanda = Some("WTICO_USD".into());
    usd.symbols.tradenation = None;
    assert!(
        same_underlying(&oil, &usd),
        "disjoint broker legs still name WTI/USD"
    );
}

#[test]
fn native_suffixes_are_preserved_and_ambiguous_aliases_are_refused() {
    logging();
    let silver = asset("XAGUSD.a");
    let listed = listing(&["XAGUSD.a", "XAGUSD.b"]);
    assert_eq!(
        select_symbol("XAGUSD.a", silver, &listed).unwrap(),
        "XAGUSD.a"
    );
    assert!(
        select_symbol("Spot Silver", silver, &listed)
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
    assert!(select_symbol("XAGUSD.c", silver, &listing(&["XAGUSD"])).is_err());
    assert!(select_symbol("XAGUSD", silver, &listing(&["EURUSD"])).is_err());
}

#[test]
fn forex_formatting_and_fractional_pips_still_work() {
    logging();
    let eur = asset("EUR_USD");
    assert_eq!(
        select_symbol("eur/usd", eur, &listing(&["EURUSD"])).unwrap(),
        "EURUSD"
    );
    let resolved = from_metadata(eur, &metadata("EURUSD", 5, 0.00001, 0)).unwrap();
    assert_eq!(resolved.precision.pip_size, 0.0001);
    let index = from_metadata(asset("SP500"), &metadata("SP500", 2, 0.01, 3)).unwrap();
    assert_eq!(index.precision.tick_size, 0.01);
    assert_eq!(index.precision.pip_size, 1.0);
}

#[test]
fn zero_tick_uses_point_and_invalid_precision_or_modes_are_refused() {
    logging();
    let silver = asset("XAGUSD");
    let resolved = from_metadata(silver, &metadata("XAGUSD", 3, 0.0, 2)).unwrap();
    assert_eq!(resolved.precision.tick_size, 0.001);
    assert!(from_metadata(silver, &metadata("XAGUSD", 3, 0.001, 1)).is_err());
    assert!(from_metadata(silver, &metadata("XAGUSD", 13, 0.001, 2)).is_err());
    assert!(from_metadata(silver, &metadata("XAGUSD", 3, f64::NAN, 2)).is_err());
    assert!(from_metadata(silver, &metadata("XAGUSD", 3, -0.001, 2)).is_err());
}

#[test]
fn mt5_resolution_requires_an_explicit_account_before_io() {
    logging();
    let error = crate::instrument_resolution::resolve_for_account(
        "MT5:XAGUSD",
        trade_control_conventions::Broker::Mt5,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("explicit named account"));
}
