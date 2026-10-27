use super::*;
use crate::{EntryError, Quote};
use serde_json::{Value, json};
use std::{cell::RefCell, future::ready};
use tracing_subscriber::prelude::*;

fn init() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn pair(name: &str, base: i64, quote: i64) -> Value {
    json!({"symbolName": name, "baseAssetId": base, "quoteAssetId": quote})
}

fn assets() -> Vec<Value> {
    vec![
        json!({"assetId": 1, "name": "AUD"}),
        json!({"assetId": 11, "name": "USD"}),
        json!({"assetId": 17, "name": "CNH"}),
    ]
}

fn cross() -> Vec<Value> {
    vec![pair("USDCNH", 11, 17), pair("AUDUSD", 1, 11)]
}

fn quote(bid: f64, ask: f64) -> Quote {
    Quote { bid, ask }
}

fn assert_conversion_error(result: std::result::Result<f64, EntryError>, detail: &str) {
    let error = result.unwrap_err();
    assert!(
        matches!(error, EntryError::CurrencyConversion { .. }),
        "{error}"
    );
    assert!(error.to_string().contains(detail), "{error}");
}

#[tokio::test]
async fn same_currency_needs_no_assets_or_quotes() {
    init();
    let rate = rate_with(
        1,
        1,
        &[],
        || async { panic!("asset lookup") },
        |_| async { panic!("quote lookup") },
    )
    .await
    .unwrap();
    assert_eq!(rate, 1.0);
}

#[tokio::test]
async fn direct_pair_takes_priority_and_preserves_bid_ask_direction() {
    init();
    let symbols = [cross(), vec![pair("AUDCNH", 1, 17)]].concat();
    for (from, to, expected) in [(17, 1, 1.0 / 4.5), (1, 17, 4.6)] {
        let rate = rate_with(
            from,
            to,
            &symbols,
            || async { panic!("direct conversion must not fetch assets") },
            |name| {
                assert_eq!(name, "AUDCNH");
                ready(Ok(quote(4.5, 4.6)))
            },
        )
        .await
        .unwrap();
        assert!((rate - expected).abs() < 1e-12);
    }
}

#[tokio::test]
async fn cnh_risk_converts_to_aud_via_brokers_usd_asset() {
    init();
    let calls = RefCell::new(Vec::new());
    let rate = rate_with(
        17,
        1,
        &cross(),
        || ready(Ok(assets())),
        |name| {
            calls.borrow_mut().push(name.to_owned());
            ready(Ok(match name {
                "USDCNH" => quote(7.1, 7.2),
                "AUDUSD" => quote(0.65, 0.66),
                _ => panic!("unexpected quote {name}"),
            }))
        },
    )
    .await
    .unwrap();
    assert!((rate - 1.0 / (7.1 * 0.65)).abs() < 1e-12);
    assert_eq!(*calls.borrow(), ["USDCNH", "AUDUSD"]);
    // A 0.01 CNH stop distance with a 100 AUD cash budget remains correctly sized.
    assert!((100.0 / (0.01 * rate) - 46_150.0).abs() < 1e-8);
}

#[tokio::test]
async fn usd_fallback_supports_both_orientations_of_each_leg() {
    init();
    for first_inverse in [false, true] {
        for second_inverse in [false, true] {
            let first = if first_inverse {
                pair("USDCNH", 11, 17)
            } else {
                pair("CNHUSD", 17, 11)
            };
            let second = if second_inverse {
                pair("AUDUSD", 1, 11)
            } else {
                pair("USDAUD", 11, 1)
            };
            let symbols = [first, second];
            let expected = (if first_inverse { 1.0 / 2.0 } else { 3.0 })
                * (if second_inverse { 1.0 / 5.0 } else { 7.0 });
            let rate = rate_with(
                17,
                1,
                &symbols,
                || ready(Ok(assets())),
                |name| {
                    ready(Ok(if name.contains("CNH") {
                        quote(2.0, 3.0)
                    } else {
                        quote(5.0, 7.0)
                    }))
                },
            )
            .await
            .unwrap();
            assert!((rate - expected).abs() < 1e-12);
        }
    }
}

#[tokio::test]
async fn missing_or_ambiguous_usd_metadata_is_refused() {
    init();
    for usd_assets in [
        vec![],
        vec![json!({"name":"USD"})],
        vec![json!({"name":"USD","assetId":0})],
        vec![
            json!({"name":"USD","assetId":11}),
            json!({"name":"USD","assetId":99}),
        ],
    ] {
        let result = rate_with(
            17,
            1,
            &cross(),
            || ready(Ok(usd_assets)),
            |_| async { panic!("quote lookup") },
        )
        .await;
        assert_conversion_error(result, "USD asset");
    }
}

#[tokio::test]
async fn missing_or_ambiguous_conversion_legs_are_refused_before_quotes() {
    init();
    for symbols in [
        vec![],
        vec![pair("USDCNH", 11, 17)],
        vec![pair("AUDUSD", 1, 11)],
        [cross(), vec![pair("USDCNH.other", 11, 17)]].concat(),
        [cross(), vec![pair("AUDUSD.other", 1, 11)]].concat(),
    ] {
        let result = rate_with(
            17,
            1,
            &symbols,
            || ready(Ok(assets())),
            |_| async { panic!("quote lookup") },
        )
        .await;
        assert_conversion_error(result, "conversion pair");
    }
}

#[tokio::test]
async fn ambiguous_direct_pair_does_not_fall_back_to_usd() {
    init();
    let symbols = [cross(), vec![pair("AUDCNH", 1, 17), pair("CNHAUD", 17, 1)]].concat();
    let result = rate_with(
        17,
        1,
        &symbols,
        || async { panic!("asset lookup") },
        |_| async { panic!("quote lookup") },
    )
    .await;
    assert_conversion_error(result, "ambiguous conversion pair");
}

#[tokio::test]
async fn missing_direct_pair_with_usd_endpoint_is_refused() {
    init();
    for (from, to) in [(11, 1), (1, 11)] {
        let result = rate_with(
            from,
            to,
            &[],
            || ready(Ok(assets())),
            |_| async { panic!("quote lookup") },
        )
        .await;
        assert_conversion_error(result, "conversion pair");
    }
}

#[tokio::test]
async fn failed_conversion_reads_are_propagated() {
    init();
    let result = rate_with(
        17,
        1,
        &cross(),
        || ready(Err(EntryError::AccountFetch)),
        |_| async { panic!("quote lookup") },
    )
    .await;
    assert!(matches!(result, Err(EntryError::AccountFetch)));
    for failed in ["USDCNH", "AUDUSD"] {
        let result = rate_with(
            17,
            1,
            &cross(),
            || ready(Ok(assets())),
            |name| {
                ready(if name == failed {
                    Err(EntryError::AccountFetch)
                } else {
                    Ok(quote(1.0, 1.1))
                })
            },
        )
        .await;
        assert!(matches!(result, Err(EntryError::AccountFetch)));
    }
}

#[tokio::test]
async fn invalid_quotes_and_cross_rate_overflow_or_underflow_are_refused() {
    init();
    for invalid in [
        quote(0.0, 1.0),
        quote(-1.0, 1.0),
        quote(2.0, 1.0),
        quote(f64::NAN, 1.0),
        quote(1.0, f64::INFINITY),
    ] {
        let result = rate_with(
            17,
            1,
            &cross(),
            || ready(Ok(assets())),
            |_| ready(Ok(invalid)),
        )
        .await;
        assert_conversion_error(result, "invalid conversion quote");
    }
    for value in [1e-200, 1e200] {
        let result = rate_with(
            17,
            1,
            &cross(),
            || ready(Ok(assets())),
            |_| ready(Ok(quote(value, value))),
        )
        .await;
        assert_conversion_error(result, "invalid conversion rate");
    }
}
