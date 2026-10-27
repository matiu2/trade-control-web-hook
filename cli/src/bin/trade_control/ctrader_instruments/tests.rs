use super::*;
use crate::{Cli, Cmd, InstrumentsCmd};
use broker_ctrader::market_data::InstrumentSummary;
use clap::Parser;

fn init_tracing() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn symbol(id: i64, name: &str) -> InstrumentSummary {
    InstrumentSummary {
        id,
        name: name.into(),
        description: None,
        enabled: Some(true),
    }
}

#[test]
fn ctrader_instruments_require_an_explicit_nonempty_account() {
    init_tracing();
    assert!(
        require_account(None)
            .unwrap_err()
            .to_string()
            .contains("--account")
    );
    assert!(require_account(Some(" ")).is_err());
    assert_eq!(
        require_account(Some("ctrader-demo")).unwrap(),
        "ctrader-demo"
    );
}

#[test]
fn resolution_uses_only_selected_catalogue_and_refuses_ambiguous_aliases() {
    init_tracing();
    let symbols = vec![
        symbol(42, "EURUSD"),
        symbol(99, "EUR/USD"),
        symbol(101, "EURUSD.c"),
    ];
    assert_eq!(resolve(&symbols, "EURUSD").unwrap().id, 42);
    assert_eq!(resolve(&symbols, "EURUSD.c").unwrap().id, 101);
    assert!(resolve(&symbols, "eur-usd").is_none());
    assert!(resolve(&symbols, "GBPUSD").is_none());
    assert!(resolve(&symbols, "").is_none());
    assert_eq!(resolve(&[symbol(75, "AUDUSD")], "aud/usd").unwrap().id, 75);
}

#[test]
fn all_instrument_operations_parse_named_ctrader_profile() {
    init_tracing();
    for operation in [vec!["list"], vec!["refresh"], vec!["resolve", "EUR/USD"]] {
        let mut arguments = vec!["trade-control", "instruments"];
        arguments.extend(operation);
        arguments.extend(["--broker", "ctrader", "--account", "ctrader-demo"]);
        let parsed = Cli::try_parse_from(arguments).unwrap();
        let Some(Cmd::Instruments(command)) = parsed.cmd else {
            panic!("instrument command expected")
        };
        let account = match command {
            InstrumentsCmd::List { account, .. }
            | InstrumentsCmd::Refresh { account, .. }
            | InstrumentsCmd::Resolve { account, .. } => account,
        };
        assert_eq!(account.as_deref(), Some("ctrader-demo"));
    }
}

#[test]
fn tradenation_instrument_default_remains_compatible() {
    init_tracing();
    let parsed = Cli::try_parse_from(["trade-control", "instruments", "list"]).unwrap();
    let Some(Cmd::Instruments(InstrumentsCmd::List {
        broker, account, ..
    })) = parsed.cmd
    else {
        panic!("list expected")
    };
    assert!(matches!(broker, BrokerArg::TradeNation));
    assert!(account.is_none());
}

#[test]
fn every_operation_refuses_missing_account_before_connecting() {
    init_tracing();
    for operation in [
        Operation::List { json: false },
        Operation::Refresh,
        Operation::Resolve {
            name: "EURUSD".into(),
            json: true,
        },
    ] {
        let error = run(operation, None).unwrap_err();
        assert!(error.to_string().contains("--account"));
    }
}

#[test]
fn demo_broker_alias_selects_exact_profile_and_refuses_conflicts() {
    init_tracing();
    let (broker, account) = BrokerArg::CtraderDemo.select(None).unwrap();
    assert_eq!(broker, BrokerKind::Ctrader);
    assert_eq!(account.as_deref(), Some("ctrader-demo"));
    assert!(
        BrokerArg::CtraderDemo
            .select(Some("different-demo".into()))
            .is_err()
    );
    assert!(
        BrokerArg::CtraderDemo
            .select(Some("ctrader-demo".into()))
            .is_ok()
    );
    let parsed = Cli::try_parse_from([
        "trade-control",
        "instruments",
        "list",
        "--broker",
        "ctrader-demo",
        "--json",
    ])
    .unwrap();
    let Some(Cmd::Instruments(InstrumentsCmd::List {
        broker,
        account,
        json,
    })) = parsed.cmd
    else {
        panic!("list expected")
    };
    assert_eq!(broker, BrokerArg::CtraderDemo);
    assert!(account.is_none());
    assert!(json);
}

#[test]
fn demo_alias_cannot_authenticate_a_live_profile() {
    init_tracing();
    let make_profile = |environment: &str| {
        Profile::OpenApi(
            serde_json::from_value(serde_json::json!({
                "client_id":"test-client", "client_secret":"private", "access_token":"private",
                "account_id":42, "environment":environment,
            }))
            .unwrap(),
        )
    };
    assert_eq!(
        profile_environment("ctrader-demo", &make_profile("demo")).unwrap(),
        "demo"
    );
    assert!(profile_environment("ctrader-demo", &make_profile("live")).is_err());
    assert_eq!(
        profile_environment("future-live", &make_profile("live")).unwrap(),
        "live"
    );
}
