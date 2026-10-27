use super::*;
use trade_control_core::account::MemMetadataStore;

pub(super) fn init() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .ok();
}

fn options(root: &std::path::Path) -> mt5::Options {
    init();
    mt5::Options {
        login: Some(42),
        server: Some("Test-Demo".into()),
        server_time: Some("+03:00".into()),
        mailbox: Some(root.join("mailbox")),
        ..mt5::Options::default()
    }
}

#[test]
fn parses_mt5_registration_and_rotation_without_password_in_argv() {
    init();
    let cli = Cli::try_parse_from([
        "trade-control-accounts",
        "add",
        "contest",
        "--broker",
        "mt5",
        "--kind",
        "demo",
        "--login",
        "42",
        "--password-stdin",
    ])
    .unwrap();
    let Command::Add(args) = cli.command else {
        panic!("expected add")
    };
    assert_eq!(args.mt5.login, Some(42));
    assert!(args.mt5.password_stdin);
    assert!(
        Cli::try_parse_from([
            "trade-control-accounts",
            "credentials",
            "contest",
            "--login",
            "43"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from([
            "trade-control-accounts",
            "credentials",
            "contest",
            "--password",
            "secret"
        ])
        .is_err()
    );
}

#[tokio::test]
async fn duplicate_add_does_not_prompt_or_change_files() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("accounts.toml");
    let store = MemMetadataStore::new();
    let meta = AccountMetadata::new("contest", BrokerKind::Mt5, AccountKind::Demo);
    store.seed(meta.clone());
    assert!(
        registration::add(&store, meta, &options(root.path()), Some(&path), || panic!(
            "must not prompt"
        ))
        .await
        .is_err()
    );
    assert!(!path.exists());
}

#[tokio::test]
async fn rotation_preserves_caps_and_updates_profile_and_credentials() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("accounts.toml");
    let store = MemMetadataStore::new();
    let mut meta = AccountMetadata::new("contest", BrokerKind::Mt5, AccountKind::Demo);
    meta.caps.max_risk_pct = Some(0.25);
    registration::add(
        &store,
        meta.clone(),
        &options(root.path()),
        Some(&path),
        || Ok("first".into()),
    )
    .await
    .unwrap();
    let rotation = mt5::Options {
        login: Some(99),
        ..mt5::Options::default()
    };
    registration::credentials(&store, "contest", &rotation, &path, || Ok("second".into()))
        .await
        .unwrap();
    assert_eq!(store.get("contest").await.unwrap(), meta);
    let config =
        mt5_data_source::account_config::AccountConfig::load_from(&path, "contest").unwrap();
    assert_eq!(config.login, 99);
    let creds: toml::Value =
        toml::from_str(&std::fs::read_to_string(config.credentials_file).unwrap()).unwrap();
    assert_eq!(creds["creds"]["username"].as_str(), Some("99"));
    assert_eq!(creds["creds"]["password"].as_str(), Some("second"));
}

#[tokio::test]
async fn rotation_rejects_unknown_foreign_and_live_accounts_before_prompting() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("accounts.toml");
    let store = MemMetadataStore::new();
    store.seed(AccountMetadata::new(
        "foreign",
        BrokerKind::Oanda,
        AccountKind::Demo,
    ));
    store.seed(AccountMetadata::new(
        "live",
        BrokerKind::Mt5,
        AccountKind::Live,
    ));
    for name in ["missing", "foreign", "live"] {
        assert!(
            registration::credentials(&store, name, &options(root.path()), &path, || panic!(
                "must not prompt"
            ))
            .await
            .is_err()
        );
    }
    assert!(!path.exists());
}

pub(super) struct FailingStore;
impl MetadataStore for FailingStore {
    async fn get(&self, name: &str) -> std::result::Result<AccountMetadata, MetadataError> {
        Err(MetadataError::NotFound(name.into()))
    }
    async fn list(&self) -> std::result::Result<Vec<AccountMetadata>, MetadataError> {
        Ok(vec![])
    }
    async fn add(&self, _: AccountMetadata) -> std::result::Result<(), MetadataError> {
        Err(MetadataError::Backend("injected insert failure".into()))
    }
    async fn remove(&self, _: &str) -> std::result::Result<(), MetadataError> {
        panic!("no metadata was added")
    }
}

#[tokio::test]
async fn metadata_failure_rolls_back_both_files() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("accounts.toml");
    let meta = AccountMetadata::new("contest", BrokerKind::Mt5, AccountKind::Demo);
    assert!(
        registration::add(
            &FailingStore,
            meta,
            &options(root.path()),
            Some(&path),
            || Ok("secret".into())
        )
        .await
        .is_err()
    );
    assert!(!path.exists());
    assert!(!root.path().join("mt5-credentials/contest.toml").exists());
}

#[tokio::test]
async fn non_mt5_add_rejects_mt5_options_without_writing_metadata() {
    init();
    let store = MemMetadataStore::new();
    let args = AddArgs {
        name: "foreign".into(),
        broker: BrokerArg::Tradenation,
        kind: KindArg::Demo,
        oanda_account_id: None,
        max_risk_pct: None,
        max_open_positions: None,
        ctrader: ctrader::Options::default(),
        risk: risk::Options::default(),
        mt5: mt5::Options {
            login: Some(42),
            ..mt5::Options::default()
        },
    };
    assert!(add(&store, args).await.is_err());
    assert!(store.list().await.unwrap().is_empty());
}

#[test]
fn parses_ctrader_identity_and_prop_backoff_without_argv_secrets() {
    init();
    let cli = Cli::try_parse_from([
        "trade-control-accounts",
        "add",
        "prop-one",
        "--broker",
        "ctrader",
        "--kind",
        "demo",
        "--ctrader-account-id",
        "123",
        "--ctrader-secrets-stdin",
        "--prop-firm",
        "--initial-balance",
        "10000",
        "--new-flat-account",
        "--daily-loss-limit-pct",
        "3",
        "--max-account-loss-pct",
        "6",
        "--daily-reference",
        "balance",
        "--drawdown-reference",
        "static-initial-balance",
        "--risk-base-pct",
        "0.5",
        "--daily-reset",
        "17:00",
        "--risk-timezone",
        "America/New_York",
        "--rules-verified",
        "--fees-verified",
        "--risk-cost",
        "EURUSD:0.00004:0.00004:0.00010",
    ])
    .unwrap();
    let Command::Add(args) = cli.command else {
        panic!("expected add")
    };
    assert_eq!(args.ctrader.account_id, Some(123));
    assert!(args.risk.prop_firm);
    assert_eq!(args.risk.base_risk_pct.unwrap().to_string(), "0.5");
    assert!(
        Cli::try_parse_from([
            "trade-control-accounts",
            "credentials",
            "prop-one",
            "--ctrader-client-secret",
            "SECRET"
        ])
        .is_err()
    );
}
