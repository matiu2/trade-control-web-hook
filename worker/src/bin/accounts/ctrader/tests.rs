use super::*;
use trade_control_core::account::MemMetadataStore;
fn init() {
    crate::tests::init();
}
fn meta() -> AccountMetadata {
    AccountMetadata::new(
        "prop-one",
        BrokerKind::Ctrader,
        trade_control_core::account::AccountKind::Demo,
    )
}
fn secrets() -> Secrets {
    Secrets {
        client_id: "CLIENT_MARKER".into(),
        client_secret: "SECRET_MARKER".into(),
        access_token: "TOKEN_MARKER".into(),
        fix: None,
    }
}
#[tokio::test]
async fn ctrader_registration_writes_private_credentials_and_preserves_other_profiles() -> Result<()>
{
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    let store = MemMetadataStore::new();
    let options = Options {
        account_id: Some(123),
        ..Default::default()
    };
    register(
        &store,
        meta(),
        &options,
        &risk::Options::default(),
        &paths,
        || Ok(secrets()),
    )
    .await?;
    let text = std::fs::read_to_string(&paths.oauth)?;
    let profiles: std::collections::BTreeMap<String, ctrader_api::Config> = toml::from_str(&text)?;
    assert_eq!(profiles["prop-one"].account_id, 123);
    assert_eq!(profiles["prop-one"].access_token, "TOKEN_MARKER");
    assert_eq!(
        store
            .get("prop-one")
            .await
            .unwrap()
            .oanda_account_id
            .as_deref(),
        Some("123")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&paths.oauth)?.permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(!format!("{:?}", secrets()).contains("MARKER"));
    Ok(())
}
#[tokio::test]
async fn duplicate_metadata_never_prompts_or_changes_credential_files() -> Result<()> {
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    let store = MemMetadataStore::new();
    store.seed(meta());
    assert!(
        register(
            &store,
            meta(),
            &Options {
                account_id: Some(123),
                ..Default::default()
            },
            &risk::Options::default(),
            &paths,
            || panic!("must not prompt")
        )
        .await
        .is_err()
    );
    assert!(!paths.oauth.exists());
    Ok(())
}
#[test]
fn secret_input_errors_never_include_values() {
    init();
    let error = read_secrets(&mut &b"{\"client_secret\":\"SECRET_MARKER\",bad}"[..])
        .err()
        .unwrap();
    assert!(!format!("{error:#}").contains("SECRET_MARKER"));
}
fn prop_options() -> risk::Options {
    risk::Options {
        prop_firm: true,
        initial_balance: Some(adaptive_risk::Decimal::from(10000)),
        new_flat_account: true,
        max_account_loss_pct: Some(adaptive_risk::Decimal::from(6)),
        daily_loss_limit_pct: Some(adaptive_risk::Decimal::from(3)),
        daily_reference: Some("balance".into()),
        drawdown_reference: Some("static-initial-balance".into()),
        rules_verified: true,
        fees_verified: true,
        costs: vec!["EURUSD:0.00004:0.00004:0.00010".into()],
        ..Default::default()
    }
}
#[tokio::test]
async fn prop_add_enrolls_risk_and_rotation_never_resets_a_streak() -> Result<()> {
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    let store = MemMetadataStore::new();
    register(
        &store,
        meta(),
        &Options {
            account_id: Some(123),
            ..Default::default()
        },
        &prop_options(),
        &paths,
        || Ok(secrets()),
    )
    .await?;
    let p = risk::load(&paths.risk, "prop-one")?.ok_or_else(|| eyre!("profile missing"))?;
    p.validate()?;
    assert_eq!(p.risk.daily_loss_limit_pct, adaptive_risk::Decimal::from(3));
    assert_eq!(p.risk.calendar.timezone.to_string(), "America/New_York");
    let state_path = p.state_directory.join("state.json");
    let mut state: adaptive_risk::RiskState = serde_json::from_slice(&std::fs::read(&state_path)?)?;
    state.consecutive_losses = 3;
    std::fs::write(&state_path, serde_json::to_vec(&state)?)?;
    let before = std::fs::read(&state_path)?;
    credentials(&store, "prop-one", &Options::default(), &paths, || {
        let mut s = secrets();
        s.access_token = "ROTATED_TOKEN".into();
        Ok(s)
    })
    .await?;
    assert_eq!(std::fs::read(&state_path)?, before);
    assert!(
        credentials(
            &store,
            "prop-one",
            &Options {
                account_id: Some(999),
                ..Default::default()
            },
            &paths,
            || Ok(secrets())
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&state_path)?, before);
    Ok(())
}
#[tokio::test]
async fn unverified_or_incomplete_prop_rules_never_prompt_for_credentials() -> Result<()> {
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    let store = MemMetadataStore::new();
    let mut risk = prop_options();
    risk.rules_verified = false;
    assert!(
        register(
            &store,
            meta(),
            &Options {
                account_id: Some(123),
                ..Default::default()
            },
            &risk,
            &paths,
            || panic!("must not prompt")
        )
        .await
        .is_err()
    );
    assert!(!paths.oauth.exists());
    assert!(store.list().await.unwrap().is_empty());
    Ok(())
}
#[tokio::test]
async fn multiple_profiles_survive_rotation_and_oauth_reuse() -> Result<()> {
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    let store = MemMetadataStore::new();
    register(
        &store,
        meta(),
        &Options {
            account_id: Some(123),
            ..Default::default()
        },
        &risk::Options::default(),
        &paths,
        || Ok(secrets()),
    )
    .await?;
    let mut second = meta();
    second.name = "second".into();
    register(
        &store,
        second,
        &Options {
            account_id: Some(124),
            reuse: Some("prop-one".into()),
            ..Default::default()
        },
        &risk::Options::default(),
        &paths,
        || panic!("reuse must not prompt"),
    )
    .await?;
    let old = std::fs::read_to_string(&paths.oauth)?;
    std::fs::write(&paths.oauth, format!("# preserved\n{old}"))?;
    credentials(&store, "prop-one", &Options::default(), &paths, || {
        let mut s = secrets();
        s.access_token = "ROTATED".into();
        Ok(s)
    })
    .await?;
    let document = std::fs::read_to_string(&paths.oauth)?;
    assert!(document.contains("# preserved"));
    let all: std::collections::BTreeMap<String, ctrader_api::Config> = toml::from_str(&document)?;
    assert_eq!(all["second"].access_token, "TOKEN_MARKER");
    assert_eq!(all["prop-one"].access_token, "ROTATED");
    Ok(())
}
#[tokio::test]
async fn prop_metadata_failure_preserves_protection_for_safe_retry() -> Result<()> {
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    assert!(
        register(
            &crate::tests::FailingStore,
            meta(),
            &Options {
                account_id: Some(123),
                ..Default::default()
            },
            &prop_options(),
            &paths,
            || Ok(secrets())
        )
        .await
        .is_err()
    );
    let p = risk::load(&paths.risk, "prop-one")?.ok_or_else(|| eyre!("risk profile missing"))?;
    let before = std::fs::read(p.state_directory.join("state.json"))?;
    let store = MemMetadataStore::new();
    register(
        &store,
        meta(),
        &Options::default(),
        &risk::Options::default(),
        &paths,
        || panic!("saved credentials must be reused"),
    )
    .await?;
    assert_eq!(std::fs::read(p.state_directory.join("state.json"))?, before);
    Ok(())
}
#[tokio::test]
async fn nonprop_metadata_failure_restores_private_files() -> Result<()> {
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    assert!(
        register(
            &crate::tests::FailingStore,
            meta(),
            &Options {
                account_id: Some(123),
                ..Default::default()
            },
            &risk::Options::default(),
            &paths,
            || Ok(secrets())
        )
        .await
        .is_err()
    );
    assert!(!paths.oauth.exists());
    Ok(())
}
fn hybrid() -> Secrets {
    let mut s = secrets();
    s.fix = Some(options::FixSecrets {
        quote: ctrader_fix::Config {
            host: "demo.example.com".into(),
            port: 5201,
            sender_comp_id: "demo.example.42".into(),
            target_comp_id: "cServer".into(),
            sender_sub_id: "QUOTE".into(),
            password: "QUOTE_MARKER".into(),
        },
        trade: ctrader_fix::TradeConfig {
            host: "demo.example.com".into(),
            port: 5202,
            sender_comp_id: "demo.example.42".into(),
            target_comp_id: "cServer".into(),
            sender_sub_id: "TRADE".into(),
            password: "TRADE_MARKER".into(),
        },
        broker_name: "Example Broker".into(),
    });
    s
}
#[tokio::test]
async fn hybrid_add_stores_each_private_profile_with_distinct_openapi_and_terminal_ids()
-> Result<()> {
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    let store = MemMetadataStore::new();
    register(
        &store,
        meta(),
        &Options {
            account_id: Some(123),
            fix: true,
            ..Default::default()
        },
        &risk::Options::default(),
        &paths,
        || Ok(hybrid()),
    )
    .await?;
    assert_eq!(
        store
            .get("prop-one")
            .await
            .unwrap()
            .oanda_account_id
            .as_deref(),
        Some("42")
    );
    let binding: toml::Value = toml::from_str(&std::fs::read_to_string(&paths.companion)?)?;
    assert_eq!(
        binding["prop-one"]["open_api_account_id"].as_integer(),
        Some(123)
    );
    assert_eq!(
        binding["prop-one"]["fix_sender_comp_id"].as_str(),
        Some("demo.example.42")
    );
    for path in [&paths.oauth, &paths.quote, &paths.trade, &paths.companion] {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
        }
    }
    credentials(&store, "prop-one", &Options::default(), &paths, || {
        Ok(secrets())
    })
    .await?;
    assert!(paths.quote.exists());
    assert!(paths.trade.exists());
    Ok(())
}
#[tokio::test]
async fn invalid_hybrid_pair_never_changes_credentials_or_metadata() -> Result<()> {
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    let store = MemMetadataStore::new();
    let mut invalid = hybrid();
    invalid.fix.as_mut().unwrap().trade.sender_comp_id = "demo.example.99".into();
    assert!(
        register(
            &store,
            meta(),
            &Options {
                account_id: Some(123),
                fix: true,
                ..Default::default()
            },
            &risk::Options::default(),
            &paths,
            || Ok(invalid)
        )
        .await
        .is_err()
    );
    assert!(!paths.oauth.exists());
    assert!(store.list().await.unwrap().is_empty());
    Ok(())
}
#[tokio::test]
async fn failed_props_enrollment_never_replaces_existing_streak_or_policy() -> Result<()> {
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    let store = MemMetadataStore::new();
    register(
        &store,
        meta(),
        &Options {
            account_id: Some(123),
            ..Default::default()
        },
        &prop_options(),
        &paths,
        || Ok(secrets()),
    )
    .await?;
    let p = risk::load(&paths.risk, "prop-one")?.unwrap();
    let state = std::fs::read(p.state_directory.join("state.json"))?;
    let other_store = MemMetadataStore::new();
    let mut changed = prop_options();
    changed.base_risk_pct = Some(adaptive_risk::Decimal::new(5, 1));
    assert!(
        register(
            &other_store,
            meta(),
            &Options::default(),
            &changed,
            &paths,
            || panic!("must reuse existing credentials")
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(p.state_directory.join("state.json"))?, state);
    Ok(())
}
#[tokio::test]
async fn account_names_cannot_escape_state_directory() -> Result<()> {
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    let store = MemMetadataStore::new();
    let mut bad = meta();
    bad.name = "../escape".into();
    assert!(
        register(
            &store,
            bad,
            &Options {
                account_id: Some(123),
                ..Default::default()
            },
            &prop_options(),
            &paths,
            || panic!("must not prompt")
        )
        .await
        .is_err()
    );
    assert!(!paths.oauth.exists());
    Ok(())
}

#[tokio::test]
async fn physical_account_alias_shows_active_protection_over_disabled_named_profile() -> Result<()>
{
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    let store = MemMetadataStore::new();
    register(
        &store,
        meta(),
        &Options {
            account_id: Some(123),
            ..Default::default()
        },
        &prop_options(),
        &paths,
        || Ok(secrets()),
    )
    .await?;
    let mut alias = meta();
    alias.name = "alias".into();
    register(
        &store,
        alias,
        &Options {
            account_id: Some(123),
            reuse: Some("prop-one".into()),
            ..Default::default()
        },
        &risk::Options::default(),
        &paths,
        || panic!("must reuse credentials"),
    )
    .await?;
    let mut disabled = risk::load(&paths.risk, "prop-one")?.unwrap();
    disabled.enabled = false;
    let mut doc = files::read_document(&paths.risk, false)?;
    let table = toml::to_string(&disabled)?.parse::<toml_edit::DocumentMut>()?;
    doc["accounts"]["alias"] = toml_edit::Item::Table(table.as_table().clone());
    std::fs::write(&paths.risk, doc.to_string())?;
    assert!(risk::display_profile(&paths, "alias")?.unwrap().enabled);
    Ok(())
}

#[tokio::test]
async fn existing_fix_management_profile_registers_without_oauth_or_file_changes() -> Result<()> {
    init();
    let root = tempfile::tempdir()?;
    let paths = Paths::in_root(root.path());
    let store = MemMetadataStore::new();
    let write = |path: &std::path::Path, text: String| -> Result<()> {
        let change = files::Change::read(path.to_owned())?;
        change.install(change.stage(text.as_bytes(), true)?)
    };
    write(
        &paths.quote,
        "[prop-one]\nhost = \"demo.example.com\"\nport = 5201\nsender_comp_id = \"demo.example.42\"\ntarget_comp_id = \"cServer\"\nsender_sub_id = \"QUOTE\"\npassword = \"QUOTE_MARKER\"\n".into(),
    )?;
    write(
        &paths.trade,
        "[prop-one]\nhost = \"demo.example.com\"\nport = 5202\nsender_comp_id = \"demo.example.42\"\ntarget_comp_id = \"cServer\"\nsender_sub_id = \"TRADE\"\npassword = \"TRADE_MARKER\"\n".into(),
    )?;
    let before = std::fs::read(&paths.trade)?;
    register(
        &store,
        meta(),
        &Options::default(),
        &risk::Options::default(),
        &paths,
        || panic!("existing FIX management must not prompt for OAuth"),
    )
    .await?;
    assert_eq!(
        store
            .get("prop-one")
            .await
            .unwrap()
            .oanda_account_id
            .as_deref(),
        Some("42")
    );
    assert_eq!(std::fs::read(&paths.trade)?, before);
    assert!(!paths.oauth.exists());
    Ok(())
}
