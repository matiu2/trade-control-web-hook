use super::*;
use chrono::TimeZone;
use tracing_subscriber::prelude::*;

fn plan(symbol: &str, accounts: &[&str]) -> TradePlan {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    serde_json::from_value(serde_json::json!({
        "trade_id":"spread", "instrument":symbol, "direction":"long",
        "granularity":"h1", "pip_size":0.0001,
        "rules":accounts.iter().enumerate().map(|(index, account)| serde_json::json!({
            "rule_id":index.to_string(), "trigger":{"type":"time_reached", "at_epoch":1},
            "fire_mode":"once", "intent":{
                "v":1, "id":index.to_string(), "not_after":"2027-01-01T00:00:00Z",
                "action":"veto", "instrument":symbol, "broker":"ctrader", "account":account
            }
        })).collect::<Vec<_>>()
    }))
    .unwrap()
}

#[test]
fn legacy_and_empty_rule_plans_keep_existing_symbol_clocks() {
    let at = Utc.with_ymd_and_hms(2026, 6, 16, 21, 0, 0).unwrap();
    for (symbol, accounts) in [
        ("EUR_USD", vec!["m-and-w"]),
        ("EUR/USD", vec!["dev"]),
        ("EURUSD", vec![]),
    ] {
        let plan = plan(symbol, &accounts);
        assert_eq!(keys(&plan), BTreeSet::from([Cow::Borrowed(symbol)]));
        assert_eq!(
            suppress_plan(&plan, at),
            spread_blackout::suppress_on_spread_hour(symbol, at, plan.granularity)
        );
        assert_eq!(
            block_window(&plan, at),
            spread_blackout::spread_block_window(symbol, at)
        );
    }
}

#[test]
fn named_account_keys_are_deduplicated_without_renaming_native_symbols() {
    for (account, key) in [
        ("ctrader-demo", "ctrader:ctrader-demo:EURUSD"),
        ("the5ers-competition", "mt5:the5ers-competition:EURUSD"),
    ] {
        let plan = plan("EURUSD", &[account, account]);
        assert_eq!(keys(&plan), BTreeSet::from([Cow::Borrowed(key)]));
        assert_eq!(plan.instrument, "EURUSD");
        let at = Utc.with_ymd_and_hms(2026, 6, 16, 21, 0, 0).unwrap();
        assert_eq!(
            block_window(&plan, at),
            spread_blackout::spread_block_window(key, at)
        );
    }
}

#[test]
fn multiple_accounts_union_clocks_and_ignore_rules_for_other_instruments() {
    let mut plan = plan("EURUSD", &["unmeasured", "ctrader-demo", "ctrader-demo"]);
    let mut foreign = plan.rules[0].clone();
    foreign.intent.instrument = "GBPUSD".into();
    foreign.intent.account = Some("another-account".into());
    plan.rules.push(foreign);
    assert_eq!(keys(&plan).len(), 2);
    let at = Utc.with_ymd_and_hms(2026, 6, 16, 20, 45, 0).unwrap();
    assert!(!spread_blackout::is_spread_hour("EURUSD", at));
    assert!(suppress_plan(&plan, at));
    assert!(!suppress_intent(
        &plan.rules[0].intent,
        at,
        plan.granularity
    ));
    assert!(suppress_intent(&plan.rules[1].intent, at, plan.granularity));
    plan.rules.reverse();
    assert!(suppress_plan(&plan, at));
}
