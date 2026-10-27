use super::*;
use tracing_subscriber::prelude::*;

fn logging() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

#[test]
fn named_mt5_and_ctrader_charts_infer_broker_and_account() {
    logging();
    for (symbol, expected, name) in [
        (
            "ctrader-ctrader-demo:XPDUSD",
            Broker::Ctrader,
            "ctrader-demo",
        ),
        (
            "mt5-the5ers-competition:XPDUSD",
            Broker::Mt5,
            "the5ers-competition",
        ),
    ] {
        let selected = broker(None, None, symbol).unwrap();
        assert_eq!(selected, expected);
        assert_eq!(account(None, None, selected, symbol).unwrap(), name);
        assert_eq!(account(None, Some(" "), selected, symbol).unwrap(), name);
    }
}

#[test]
fn broker_explicit_and_environment_choices_outrank_chart_inference() {
    logging();
    let chart = "ctrader-ctrader-demo:XPDUSD";
    assert_eq!(
        broker(Some(Broker::Mt5), Some("oanda"), chart).unwrap(),
        Broker::Mt5
    );
    assert_eq!(
        broker(Some(Broker::Ctrader), Some("invalid"), chart).unwrap(),
        Broker::Ctrader
    );
    assert_eq!(broker(None, Some(" oanda "), chart).unwrap(), Broker::Oanda);
    assert_eq!(broker(None, Some(" "), chart).unwrap(), Broker::Ctrader);
    assert!(broker(None, Some("not-a-broker"), chart).is_err());
}

#[test]
fn account_explicit_and_environment_choices_outrank_named_charts() {
    logging();
    for (selected, chart) in [
        (Broker::Ctrader, "ctrader-ctrader-demo:XPDUSD"),
        (Broker::Mt5, "mt5-the5ers-competition:XPDUSD"),
    ] {
        assert_eq!(
            account(
                Some("cli-account"),
                Some("environment-account"),
                selected,
                chart
            )
            .unwrap(),
            "cli-account"
        );
        assert_eq!(
            account(None, Some(" environment-account "), selected, chart).unwrap(),
            "environment-account"
        );
    }
}

#[test]
fn chart_account_is_inferred_only_for_the_selected_broker() {
    logging();
    let ctrader = "ctrader-ctrader-demo:XPDUSD";
    let mt5 = "mt5-the5ers-competition:XPDUSD";
    assert_eq!(
        account(None, None, Broker::Oanda, ctrader).unwrap(),
        "m-and-w"
    );
    assert_eq!(
        account(None, None, Broker::TradeNation, ctrader).unwrap(),
        "reversals"
    );
    assert!(account(None, None, Broker::Mt5, ctrader).is_err());
    assert!(account(None, None, Broker::Ctrader, mt5).is_err());
}

#[test]
fn named_ctrader_route_preserves_replay_drawing_identity() {
    logging();
    let symbol = "ctrader-ctrader-demo:XPDUSD";
    let selected = broker(None, None, symbol).unwrap();
    let account = account(None, None, selected, symbol).unwrap();
    let feed = local_chart_client::chart_feed(selected.as_str(), &account);
    assert_eq!(feed, "ctrader-ctrader-demo");
    assert_eq!(
        local_chart_client::local_chart_symbol_for_feed("XPDUSD", Some(&feed)),
        "XPDUSD"
    );
    assert_eq!(
        local_chart_client::local_chart_tf("1h").as_deref(),
        Some("h1")
    );
}

#[test]
fn account_overrides_change_the_named_replay_feed() {
    logging();
    let chart = "ctrader-ctrader-demo:XPDUSD";
    for (explicit, environment, expected) in [
        (
            Some("cli-account"),
            Some("environment-account"),
            "ctrader-cli-account",
        ),
        (
            None,
            Some("environment-account"),
            "ctrader-environment-account",
        ),
    ] {
        let selected = broker(None, None, chart).unwrap();
        let account = account(explicit, environment, selected, chart).unwrap();
        assert_eq!(
            local_chart_client::chart_feed(selected.as_str(), &account),
            expected
        );
    }
}

#[test]
fn malformed_named_ctrader_charts_do_not_default_to_oanda() {
    logging();
    for chart in [
        "ctrader-:XPDUSD",
        "ctrader-../demo:XPDUSD",
        "ctrader-demo:",
        "ctrader-demo",
        "CTRADER-demo:XPDUSD",
    ] {
        assert!(broker(None, None, chart).is_err(), "must refuse {chart}");
        assert_eq!(
            broker(Some(Broker::Oanda), None, chart).unwrap(),
            Broker::Oanda
        );
    }
}

#[test]
fn legacy_exchange_inference_and_defaults_remain_unchanged() {
    logging();
    for (chart, expected, name) in [
        ("OANDA:EURUSD", Broker::Oanda, "m-and-w"),
        ("TRADENATION:EURUSD", Broker::TradeNation, "reversals"),
        ("EURUSD", Broker::Oanda, "m-and-w"),
        ("FX:EURUSD", Broker::Oanda, "m-and-w"),
    ] {
        let selected = broker(None, None, chart).unwrap();
        assert_eq!(selected, expected);
        assert_eq!(account(None, None, selected, chart).unwrap(), name);
    }
}
