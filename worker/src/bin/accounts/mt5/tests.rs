use super::*;
use std::{fs, path::Path};
use tempfile::TempDir;

fn init() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .ok();
}

fn options(root: &Path) -> Options {
    Options {
        login: Some(42),
        server: Some("Test-Demo".into()),
        server_time: Some("+03:00".into()),
        mailbox: Some(root.join("mailbox")),
        ..Options::default()
    }
}

fn fixture() -> (TempDir, std::path::PathBuf) {
    init();
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("mt5-accounts.toml");
    (root, config)
}

#[test]
fn new_profile_writes_matching_login_and_private_escaped_credentials() {
    let (root, path) = fixture();
    let password = " spaces \" and \\ and\nnewlines ";
    let update = Update::prepare(&path, "competition", &options(root.path())).unwrap();
    let credentials_path = update.credentials_path().to_owned();
    update.apply(password).unwrap();
    let config =
        mt5_data_source::account_config::AccountConfig::load_from(&path, "competition").unwrap();
    assert_eq!(config.login, 42);
    assert_eq!(config.credentials_file, credentials_path);
    let text = fs::read_to_string(&credentials_path).unwrap();
    let creds: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(
        creds["creds"]["username"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap(),
        config.login
    );
    assert_eq!(creds["creds"]["password"].as_str().unwrap(), password);
    assert!(!fs::read_to_string(path).unwrap().contains(password));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(credentials_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn rotation_preserves_other_profiles_comments_and_mailbox() {
    let (root, path) = fixture();
    Update::prepare(&path, "competition", &options(root.path()))
        .unwrap()
        .apply("first")
        .unwrap();
    let mut other = options(root.path());
    other.login = Some(43);
    Update::prepare(&path, "other", &other)
        .unwrap()
        .apply("other password")
        .unwrap();
    let before = format!(
        "# keep this comment\n{}",
        fs::read_to_string(&path).unwrap()
    );
    fs::write(&path, before).unwrap();
    let rotation = Options {
        login: Some(99),
        ..Options::default()
    };
    Update::prepare(&path, "competition", &rotation)
        .unwrap()
        .apply("replacement")
        .unwrap();
    let config =
        mt5_data_source::account_config::AccountConfig::load_from(&path, "competition").unwrap();
    assert_eq!(config.login, 99);
    assert_eq!(config.mailbox, root.path().join("mailbox"));
    let other = mt5_data_source::account_config::AccountConfig::load_from(&path, "other").unwrap();
    assert_eq!(other.login, 43);
    let other_creds: toml::Value =
        toml::from_str(&fs::read_to_string(other.credentials_file).unwrap()).unwrap();
    assert_eq!(
        other_creds["creds"]["password"].as_str(),
        Some("other password")
    );
    let creds =
        toml::from_str::<toml::Value>(&fs::read_to_string(config.credentials_file).unwrap())
            .unwrap();
    assert_eq!(creds["creds"]["username"].as_str(), Some("99"));
    assert!(
        fs::read_to_string(path)
            .unwrap()
            .contains("# keep this comment")
    );
}

#[test]
fn rollback_restores_both_existing_files() {
    let (root, path) = fixture();
    Update::prepare(&path, "competition", &options(root.path()))
        .unwrap()
        .apply("first")
        .unwrap();
    let original = fs::read(&path).unwrap();
    let update = Update::prepare(
        &path,
        "competition",
        &Options {
            login: Some(99),
            ..Options::default()
        },
    )
    .unwrap();
    let credential_path = update.credentials_path().to_owned();
    let original_credentials = fs::read(&credential_path).unwrap();
    update.apply("replacement").unwrap().rollback().unwrap();
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(fs::read(credential_path).unwrap(), original_credentials);
}

#[test]
fn rollback_of_new_profile_removes_new_files() {
    let (root, path) = fixture();
    let update = Update::prepare(&path, "competition", &options(root.path())).unwrap();
    let credential_path = update.credentials_path().to_owned();
    update.apply("first").unwrap().rollback().unwrap();
    assert!(!path.exists());
    assert!(!credential_path.exists());
}

#[test]
fn empty_password_changes_neither_file() {
    let (root, path) = fixture();
    assert!(
        Update::prepare(&path, "competition", &options(root.path()))
            .unwrap()
            .apply("")
            .is_err()
    );
    assert!(!path.exists());
}

#[test]
fn invalid_profile_is_rejected_before_writing() {
    let (root, path) = fixture();
    let mut invalid = options(root.path());
    invalid.server_time = Some("invalid".into());
    assert!(Update::prepare(&path, "competition", &invalid).is_err());
    assert!(Update::prepare(&path, "../escape", &options(root.path())).is_err());
    invalid = options(root.path());
    invalid.credentials_file = Some(path.clone());
    assert!(Update::prepare(&path, "competition", &invalid).is_err());
    invalid.credentials_file = Some("relative.toml".into());
    assert!(Update::prepare(&path, "competition", &invalid).is_err());
    assert!(!path.exists());
}

#[test]
fn refuses_to_overwrite_a_credential_file_shared_with_another_profile() {
    let (root, path) = fixture();
    Update::prepare(&path, "competition", &options(root.path()))
        .unwrap()
        .apply("first")
        .unwrap();
    let mut config = fs::read_to_string(&path)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    config["accounts"]["other"] = config["accounts"]["competition"].clone();
    fs::write(&path, config.to_string()).unwrap();
    let before = fs::read(&path).unwrap();
    assert!(Update::prepare(&path, "competition", &Options::default()).is_err());
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn concurrent_updates_are_rejected_instead_of_overwriting_profiles() {
    let (root, path) = fixture();
    let _first = Update::prepare(&path, "competition", &options(root.path())).unwrap();
    assert!(Update::prepare(&path, "other", &options(root.path())).is_err());
}

#[test]
fn failed_profile_write_restores_the_previous_credential_file() {
    let (root, path) = fixture();
    let mut options = options(root.path());
    let credentials = root.path().join("existing-creds.toml");
    fs::write(&credentials, "original bytes").unwrap();
    options.credentials_file = Some(credentials.clone());
    let update = Update::prepare(&path, "competition", &options).unwrap();
    // Simulate a destination becoming unwritable after staging preparation.
    fs::create_dir(&path).unwrap();
    assert!(update.apply("replacement").is_err());
    assert_eq!(fs::read_to_string(credentials).unwrap(), "original bytes");
}

#[cfg(unix)]
#[test]
fn symlink_destinations_are_rejected_without_touching_the_target() {
    use std::os::unix::fs::symlink;
    let (root, path) = fixture();
    let target = root.path().join("target");
    fs::write(&target, "original").unwrap();
    let credentials = root.path().join("linked-creds");
    symlink(&target, &credentials).unwrap();
    let mut options = options(root.path());
    options.credentials_file = Some(credentials);
    assert!(Update::prepare(&path, "competition", &options).is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "original");
}
