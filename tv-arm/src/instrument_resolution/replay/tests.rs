use super::*;
use clap::Parser;
use tracing_subscriber::prelude::*;

fn logging() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .ok();
}

#[test]
fn only_pattern_replay_can_use_non_executable_contract_metadata() {
    logging();
    for tokens in [
        vec!["tv-arm"],
        vec!["tv-arm", "register"],
        vec!["tv-arm", "plan-out", "/tmp/unused-plan.json"],
        vec!["tv-arm", "--market-entry"],
        vec!["tv-arm", "--market-entry", "replay"],
    ] {
        let args = crate::args::Args::try_parse_from(tokens).unwrap();
        assert_eq!(
            ResolutionPurpose::for_args(&args),
            ResolutionPurpose::Trading
        );
    }
    let args = crate::args::Args::try_parse_from(["tv-arm", "--trend", "replay"]).unwrap();
    assert_eq!(
        ResolutionPurpose::for_args(&args),
        ResolutionPurpose::Replay
    );
}

#[test]
fn every_non_fx_replay_requires_its_own_account_spread_profile() {
    logging();
    for symbol in ["XAUEUR", "AU200", "DE10YB", "XPDUSD", "BTCUSD", "3I"] {
        let error = require_calibrated_replay("unmeasured-test-account", symbol).unwrap_err();
        let message = error.to_string();
        let key = format!("ctrader:unmeasured-test-account:{symbol}");
        assert!(message.contains(&key), "{message}");
        assert!(message.contains("calibrat"), "{message}");
        assert!(message.contains("cTrader replay requires"), "{message}");
    }
}

#[test]
fn measured_native_account_profile_passes_without_another_brokers_mapping() {
    logging();
    require_calibrated_replay("ctrader-demo", "EURUSD").unwrap();
}
