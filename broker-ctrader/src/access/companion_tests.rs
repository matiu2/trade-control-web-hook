use super::{CompanionBinding, companions};
use color_eyre::eyre::Result;
use std::{fs, path::Path};

fn document() -> &'static str {
    "[test]\nopen_api_account_id=456\nfix_sender_comp_id='demo.example.123'\nbroker_name='Example Broker'\n"
}

fn write(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[test]
fn private_registry_loads_bound_identity_and_missing_is_absent() -> Result<()> {
    super::tests::init();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("companions.toml");
    assert!(companions::read(&path)?.is_empty());
    write(&path, document())?;
    let profiles = companions::read(&path)?;
    let binding = profiles.get("test").expect("test binding");
    assert_eq!(binding.open_api_account_id, 456);
    assert_eq!(binding.fix_sender_comp_id, "demo.example.123");
    assert_eq!(binding.broker_name, "Example Broker");
    assert!(!profiles.contains_key("missing"));
    Ok(())
}

#[test]
fn malformed_and_oversized_registry_errors_never_echo_source() -> Result<()> {
    super::tests::init();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("companions.toml");
    for content in ["secret-marker malformed TOML".into(), "a".repeat(1_048_577)] {
        write(&path, &content)?;
        assert!(!format!("{:?}", companions::read(&path).unwrap_err()).contains("secret-marker"));
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn insecure_companion_registry_is_not_treated_as_absent() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    super::tests::init();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("companions.toml");
    write(&path, document())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
    assert!(companions::read(&path).is_err());
    Ok(())
}

#[test]
fn invalid_binding_fields_and_profile_names_fail_closed() -> Result<()> {
    super::tests::init();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("companions.toml");
    for content in [
        document().replace("[test]", "['bad.name']"),
        document().replace("456", "0"),
        document().replace("Example Broker", ""),
        document()
            .replace("Example Broker", "bad\\nname")
            .replace("'bad\\nname'", "\"bad\\nname\""),
        format!("{}unknown='secret-marker'\n", document()),
    ] {
        write(&path, &content)?;
        assert!(companions::read(&path).is_err());
    }
    assert!(
        CompanionBinding {
            open_api_account_id: 456,
            fix_sender_comp_id: "demo.example.123".into(),
            broker_name: "x".repeat(257)
        }
        .validate()
        .is_err()
    );
    Ok(())
}
