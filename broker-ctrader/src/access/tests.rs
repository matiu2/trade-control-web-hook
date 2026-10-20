use super::*;

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
    assert!(fix().validate(false, Some("123")).is_ok());
    assert!(fix().validate(false, None).is_ok());
    assert!(fix().validate(true, None).is_err());
    assert!(fix().validate(false, Some("124")).is_err());
}

#[test]
fn fix_has_quotes_without_execution_or_history() {
    let profile = fix();
    let caps = profile.capabilities();
    assert!(caps.quotes);
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
    assert!(profile.capabilities().history);
    assert!(profile.require_execution().is_ok());
}

#[test]
fn unresolved_capabilities_fail_closed() {
    let caps = Capabilities::unavailable("configuration unavailable".into());
    assert!(!caps.quotes && !caps.execution && !caps.history);
    assert_eq!(caps.readiness, "unavailable");
}

#[test]
fn malformed_oauth_never_loads_fix_and_missing_oauth_does() {
    let result = Profile::resolve(Err(color_eyre::eyre::eyre!("malformed OAuth")), || {
        panic!("FIX fallback must not run")
    });
    assert!(result.unwrap_err().to_string().contains("malformed OAuth"));
    let Profile::Fix(config) = fix() else {
        panic!("FIX fixture")
    };
    assert!(matches!(
        Profile::resolve(Ok(None), || Ok(Some(config))),
        Ok(Profile::Fix(_))
    ));
}
