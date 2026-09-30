//! Pins [`handle_register`]'s arm-time account checks. The bug that motivated
//! them: `hs-aud-usd-d6ca1271` was armed `broker: oanda` on the TradeNation
//! `experimental` account, register answered `200 ok`, and the engine then
//! failed every candle fetch for six weeks without firing a single rule.

use chrono::TimeZone;

use super::*;
use crate::account::{AccountKind, AccountMetadata, MemMetadataStore};
use crate::broker::Candle;
use crate::intent::{BrokerKind, Intent, Shell};
use crate::state::{MemStateStore, StateStore};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 9, 3, 35).unwrap()
}

/// A `register` carrier for a one-rule AUD_USD plan. `account` names the
/// carrier's account (or none); `broker` is the rule intent's broker.
fn register_verified(account: Option<&str>, broker: &str) -> Verified {
    let account_field = account
        .map(|a| format!(r#""account":"{a}","#))
        .unwrap_or_default();
    let json = format!(
        r#"{{"v":1,"id":"hs-aud-usd-d6ca1271-register","not_after":"2026-08-18T09:08:35Z",
            "action":"register","instrument":"AUD_USD",{account_field}
            "trade_id":"hs-aud-usd-d6ca1271",
            "trade_plan":{{"trade_id":"hs-aud-usd-d6ca1271","instrument":"AUD_USD",
              "direction":"short","granularity":"h1","pip_size":0.0001,"rules":[
              {{"rule_id":"01-veto-too-high","fire_mode":"once","kind":"setup_invalidation",
                "trigger":{{"type":"horizontal_cross","level":0.7113,"dir":"up","bar":"on_close"}},
                "intent":{{"v":1,"id":"hs-aud-usd-d6ca1271-too-high","action":"veto",
                  "instrument":"AUD_USD","broker":"{broker}","account":"experimental",
                  "name":"too-high","ttl_hours":42,"level":"close-positions",
                  "trade_id":"hs-aud-usd-d6ca1271","not_after":"2026-08-20T02:30:00Z"}}}}]}}}}"#
    );
    let intent: Intent = serde_json::from_str(&json).expect("valid register intent");
    let shell = Shell::from_candle(&Candle {
        time: now(),
        o: 0.71,
        h: 0.71,
        l: 0.71,
        c: 0.71,
    });
    Verified { shell, intent }
}

fn tn_experimental() -> MemMetadataStore {
    let accounts = MemMetadataStore::new();
    accounts.seed(AccountMetadata::new(
        "experimental",
        BrokerKind::TradeNation,
        AccountKind::Demo,
    ));
    accounts
}

fn is_registered(store: &MemStateStore, account: Option<&str>) -> bool {
    pollster::block_on(store.get_trade_plan(account, "hs-aud-usd-d6ca1271"))
        .expect("get_trade_plan")
        .is_some()
}

/// THE BUG: an OANDA-routed plan on a TradeNation account is rejected loudly
/// and never persisted.
#[test]
fn oanda_plan_on_tradenation_account_is_rejected() {
    let store = MemStateStore::new();
    let verified = register_verified(Some("experimental"), "oanda");
    let r = pollster::block_on(handle_register(
        &store,
        &tn_experimental(),
        &verified,
        now(),
    ));
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("01-veto-too-high (Oanda)"), "{}", r.body);
    assert!(!is_registered(&store, Some("experimental")));
}

#[test]
fn matching_broker_registers() {
    let store = MemStateStore::new();
    let verified = register_verified(Some("experimental"), "tradenation");
    let r = pollster::block_on(handle_register(
        &store,
        &tn_experimental(),
        &verified,
        now(),
    ));
    assert!(r.is_success(), "{}", r.body);
    assert!(is_registered(&store, Some("experimental")));
}

/// A named account the worker doesn't know can never tick — reject it too.
#[test]
fn unknown_account_is_rejected() {
    let store = MemStateStore::new();
    let verified = register_verified(Some("no-such-account"), "tradenation");
    let r = pollster::block_on(handle_register(
        &store,
        &tn_experimental(),
        &verified,
        now(),
    ));
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("no-such-account"), "{}", r.body);
    assert!(!is_registered(&store, Some("no-such-account")));
}

/// No regression: an unscoped carrier (no account) registers as before.
#[test]
fn unscoped_plan_still_registers() {
    let store = MemStateStore::new();
    let verified = register_verified(None, "oanda");
    let r = pollster::block_on(handle_register(
        &store,
        &tn_experimental(),
        &verified,
        now(),
    ));
    assert!(r.is_success(), "{}", r.body);
    assert!(is_registered(&store, None));
}
