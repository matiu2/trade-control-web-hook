use super::*;
use tracing_subscriber::prelude::*;

pub(super) fn init() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn hybrid() -> Profile {
    let Profile::FixTrading { quote, trade } = trading() else {
        panic!("FIX fixture")
    };
    Profile::Hybrid {
        open_api: ctrader_api::Config {
            client_id: "client".into(),
            client_secret: "secret".into(),
            access_token: "token".into(),
            account_id: 456,
            environment: ctrader_api::Environment::Demo,
        },
        binding: CompanionBinding {
            open_api_account_id: 456,
            fix_sender_comp_id: quote.sender_comp_id.clone(),
            broker_name: "Example Broker".into(),
        },
        quote,
        trade,
    }
}

#[test]
fn explicit_hybrid_binding_pins_both_identifiers_and_enables_companion_capabilities() {
    init();
    let profile = hybrid();
    assert!(profile.validate(false, Some("123")).is_ok());
    assert!(profile.validate(false, Some("456")).is_err());
    let caps = profile.capabilities();
    assert!(caps.quotes && caps.management && caps.execution && caps.history);
    assert_eq!(caps.readiness, "configured-fix-open-api");
    assert_eq!(profile.require_execution().unwrap().account_id, 456);
}

#[test]
fn invalid_hybrid_binding_never_enables_execution() {
    init();
    for mismatch in ["ctid", "fix", "broker", "environment", "trade"] {
        let Profile::Hybrid {
            mut open_api,
            quote,
            mut trade,
            mut binding,
        } = hybrid()
        else {
            panic!("hybrid fixture")
        };
        match mismatch {
            "ctid" => binding.open_api_account_id = 123,
            "fix" => binding.fix_sender_comp_id = "demo.other.123".into(),
            "broker" => binding.broker_name.clear(),
            "environment" => open_api.environment = ctrader_api::Environment::Live,
            "trade" => trade.sender_comp_id = "demo.example.124".into(),
            _ => unreachable!(),
        }
        let profile = Profile::Hybrid {
            open_api,
            quote,
            trade,
            binding,
        };
        assert!(profile.validate(false, Some("123")).is_err(), "{mismatch}");
        assert!(profile.require_execution().is_err(), "{mismatch}");
    }
}

#[test]
fn explicit_binding_requires_all_three_profiles_and_never_falls_back() {
    init();
    for missing in ["oauth", "quote", "trade"] {
        let Profile::Hybrid {
            open_api,
            quote,
            trade,
            binding,
        } = hybrid()
        else {
            panic!("hybrid fixture")
        };
        let result = Profile::resolve_bound(
            Ok(Some(binding)),
            Ok((missing != "oauth").then_some(open_api)),
            || Ok((missing != "quote").then_some(quote)),
            || Ok((missing != "trade").then_some(trade)),
        );
        assert!(result.is_err(), "{missing}");
    }
}

#[test]
fn malformed_binding_prevents_oauth_fallback_and_valid_binding_selects_hybrid() {
    init();
    let Profile::Hybrid {
        open_api,
        quote,
        trade,
        binding,
    } = hybrid()
    else {
        panic!("hybrid fixture")
    };
    assert!(
        Profile::resolve_bound(
            Err(color_eyre::eyre::eyre!("malformed binding")),
            Ok(Some(open_api.clone())),
            || panic!("no FIX read"),
            || panic!("no TRADE read")
        )
        .is_err()
    );
    assert!(matches!(
        Profile::resolve_bound(
            Ok(Some(binding)),
            Ok(Some(open_api)),
            || Ok(Some(quote)),
            || Ok(Some(trade))
        ),
        Ok(Profile::Hybrid { .. })
    ));
}

fn trading() -> Profile {
    let Profile::Fix(quote) = fix() else {
        panic!("FIX fixture")
    };
    let trade = ctrader_fix::TradeConfig {
        host: quote.host.clone(),
        port: 5202,
        sender_comp_id: quote.sender_comp_id.clone(),
        target_comp_id: quote.target_comp_id.clone(),
        sender_sub_id: "TRADE".into(),
        password: "private-test".into(),
    };
    Profile::FixTrading { quote, trade }
}

#[test]
fn fix_trading_exposes_management_without_protected_entries_or_history() {
    init();
    let profile = trading();
    assert!(profile.validate(false, Some("123")).is_ok());
    let caps = profile.capabilities();
    assert!(caps.quotes && caps.management);
    assert!(!caps.execution && !caps.history);
    assert_eq!(caps.readiness, "fix-management");
    assert!(profile.require_execution().is_err());
}

#[test]
fn paired_fix_credentials_must_pin_the_same_account_target_and_demo_environment() {
    init();
    for mismatch in ["account", "target", "session", "live"] {
        let Profile::FixTrading {
            mut quote,
            mut trade,
        } = trading()
        else {
            panic!("FIX fixture")
        };
        match mismatch {
            "account" => trade.sender_comp_id = "demo.example.124".into(),
            "target" => trade.target_comp_id = "OtherServer".into(),
            "session" => trade.sender_sub_id = "QUOTE".into(),
            "live" => {
                quote.sender_comp_id = "live.example.123".into();
                trade.sender_comp_id = quote.sender_comp_id.clone();
            }
            _ => unreachable!(),
        }
        let profile = Profile::FixTrading { quote, trade };
        assert!(
            profile.validate(mismatch == "live", Some("123")).is_err(),
            "{mismatch}"
        );
    }
}

#[test]
fn malformed_trade_profile_never_degrades_to_quote_only() {
    init();
    let Profile::Fix(quote) = fix() else {
        panic!("FIX fixture")
    };
    let error =
        Profile::resolve(Ok(None), || Ok(Some(quote)), || bail!("malformed TRADE")).unwrap_err();
    assert!(error.to_string().contains("malformed TRADE"));
}

#[test]
fn optional_trade_profile_adds_management_only_when_present() {
    init();
    let Profile::FixTrading { quote, trade } = trading() else {
        panic!("FIX fixture")
    };
    assert!(matches!(
        Profile::resolve(Ok(None), || Ok(Some(quote)), || Ok(Some(trade))),
        Ok(Profile::FixTrading { .. })
    ));
}

fn fix() -> Profile {
    Profile::Fix(ctrader_fix::Config {
        host: "demo.example.com".into(),
        port: 5211,
        sender_comp_id: "demo.example.123".into(),
        target_comp_id: "cServer".into(),
        sender_sub_id: "QUOTE".into(),
        password: "private-test".into(),
    })
}

#[test]
fn fix_registration_pins_environment_and_login() {
    init();
    assert!(fix().validate(false, Some("123")).is_ok());
    assert!(fix().validate(false, None).is_ok());
    assert!(fix().validate(true, None).is_err());
    assert!(fix().validate(false, Some("124")).is_err());
}

#[test]
fn fix_has_quotes_without_execution_or_history() {
    init();
    let profile = fix();
    let caps = profile.capabilities();
    assert!(caps.quotes);
    assert!(!caps.management);
    assert!(!caps.execution);
    assert!(!caps.history);
    assert!(
        profile
            .require_execution()
            .unwrap_err()
            .to_string()
            .contains("Open API")
    );
}

#[test]
fn oauth_preserves_execution_history_and_account_validation() {
    init();
    let profile = Profile::OpenApi(ctrader_api::Config {
        client_id: "client".into(),
        client_secret: "secret".into(),
        access_token: "token".into(),
        account_id: 456,
        environment: ctrader_api::Environment::Demo,
    });
    assert!(profile.validate(false, Some("456")).is_ok());
    assert!(profile.validate(false, Some("123")).is_err());
    assert!(profile.capabilities().execution);
    assert!(profile.capabilities().management);
    assert!(profile.capabilities().history);
    assert!(profile.require_execution().is_ok());
}

#[test]
fn unresolved_capabilities_fail_closed() {
    init();
    let caps = Capabilities::unavailable("configuration unavailable".into());
    assert!(!caps.quotes && !caps.management && !caps.execution && !caps.history);
    assert_eq!(caps.readiness, "unavailable");
}

#[test]
fn valid_oauth_does_not_read_fix_profiles() {
    init();
    let config = ctrader_api::Config {
        client_id: "client".into(),
        client_secret: "secret".into(),
        access_token: "token".into(),
        account_id: 456,
        environment: ctrader_api::Environment::Demo,
    };
    assert!(matches!(
        Profile::resolve(
            Ok(Some(config)),
            || panic!("FIX must not supersede OAuth"),
            || panic!("TRADE must not supersede OAuth"),
        ),
        Ok(Profile::OpenApi(_))
    ));
}

#[test]
fn legacy_capability_payload_defaults_management_to_false() {
    init();
    let capabilities: Capabilities = serde_json::from_value(serde_json::json!({
        "quotes": true, "execution": false, "history": false,
        "readiness": "quotes-only", "reason": "legacy response"
    }))
    .unwrap();
    assert!(!capabilities.management);
}

#[test]
fn malformed_oauth_never_loads_fix_and_missing_oauth_does() {
    init();
    let result = Profile::resolve(
        Err(color_eyre::eyre::eyre!("malformed OAuth")),
        || panic!("FIX fallback must not run"),
        || panic!("TRADE fallback must not run"),
    );
    assert!(result.unwrap_err().to_string().contains("malformed OAuth"));
    let Profile::Fix(config) = fix() else {
        panic!("FIX fixture")
    };
    assert!(matches!(
        Profile::resolve(Ok(None), || Ok(Some(config)), || Ok(None)),
        Ok(Profile::Fix(_))
    ));
}
