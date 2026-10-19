use super::*;
use std::collections::VecDeque;

fn symbols(names: &[&str]) -> Vec<Symbol> {
    names
        .iter()
        .enumerate()
        .map(|(id, name)| Symbol {
            name: (*name).into(),
            id: id as u32,
            digits: 3,
        })
        .collect()
}

fn quote(bid: f64, ask: f64) -> Quote {
    Quote {
        time_msc: 1,
        bid,
        ask,
    }
}

async fn check_route(
    names: &[&str],
    from: &str,
    to: &str,
    quotes: &[(&str, f64, f64)],
    expected: f64,
) {
    crate::tests::logging();
    let mut replies: VecDeque<_> = quotes.iter().copied().collect();
    let actual = convert(&symbols(names), from, to, |name| {
        let (expected_name, bid, ask) = replies.pop_front().expect("unexpected quote request");
        assert_eq!(
            name, expected_name,
            "requested an absent or unnecessary conversion pair"
        );
        std::future::ready(Ok(quote(bid, ask)))
    })
    .await
    .unwrap();
    assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
    assert!(
        replies.is_empty(),
        "did not read the whole conversion route"
    );
}

#[tokio::test]
async fn audjpy_risk_uses_listed_usdjpy_without_requesting_missing_jpyusd() {
    check_route(
        &["AUDJPY", "USDJPY"],
        "JPY",
        "USD",
        &[("USDJPY", 158.0, 158.1)],
        1.0 / 158.0,
    )
    .await;
}

#[tokio::test]
async fn direct_conversion_uses_ask_and_does_not_probe_the_inverse() {
    check_route(
        &["CADUSD", "USDCAD"],
        "CAD",
        "USD",
        &[("CADUSD", 0.72, 0.73)],
        0.73,
    )
    .await;
}

#[tokio::test]
async fn cross_currency_conversion_selects_both_listed_usd_legs_before_reading() {
    check_route(
        &["USDJPY", "USDCAD"],
        "JPY",
        "CAD",
        &[("USDJPY", 158.0, 158.1), ("USDCAD", 1.37, 1.38)],
        1.38 / 158.0,
    )
    .await;
}

#[tokio::test]
async fn same_currency_needs_no_symbols_or_quotes() {
    check_route(&[], "USD", "USD", &[], 1.0).await;
}

#[tokio::test]
async fn missing_route_does_not_request_quotes() {
    crate::tests::logging();
    let error = convert(
        &symbols(&["USDJPY"]),
        "JPY",
        "CAD",
        |_| -> std::future::Ready<Result<Quote>> {
            panic!("no quotes may be requested when the second conversion leg is missing");
        },
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("JPY"));
    assert!(format!("{error:#}").contains("CAD"));
}

#[tokio::test]
async fn quote_failure_keeps_its_cause_and_never_probes_other_pairs() {
    crate::tests::logging();
    let mut calls = 0;
    let error = convert(&symbols(&["CADUSD", "USDCAD"]), "CAD", "USD", |name| {
        calls += 1;
        assert_eq!(name, "CADUSD");
        std::future::ready(Err(eyre!("connection reset")))
    })
    .await
    .unwrap_err();
    assert_eq!(calls, 1);
    assert!(format!("{error:#}").contains("CADUSD"));
    assert!(format!("{error:#}").contains("connection reset"));
}

#[tokio::test]
async fn invalid_bid_ask_cannot_size_an_entry() {
    crate::tests::logging();
    for (bid, ask) in [(0.0, 1.0), (1.0, f64::NAN), (1.1, 1.0)] {
        assert!(
            convert(&symbols(&["CADUSD"]), "CAD", "USD", |_| std::future::ready(
                Ok(quote(bid, ask))
            ))
            .await
            .is_err()
        );
    }
}
