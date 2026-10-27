use super::*;
fn fixture() -> tempfile::TempDir {
    crate::test_logging();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "// source\n").unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
    )
    .unwrap();
    output(root, &["init", "--quiet"]).unwrap();
    assert!(
        std::process::Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(root.join("README.md"), "documentation\n").unwrap();
    output(root, &["init", "--quiet"]).unwrap();
    output(
        root,
        &[
            "add",
            "--",
            "src/lib.rs",
            "Cargo.toml",
            "Cargo.lock",
            "README.md",
        ],
    )
    .unwrap();
    output(
        root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    )
    .unwrap();
    directory
}
#[test]
fn production_and_development_branches_cannot_deploy() {
    crate::test_logging();
    assert!(require_staging("staging").is_ok());
    assert!(require_staging("main").is_err());
    assert!(require_staging("dev").is_err());
}
#[test]
fn existing_drafts_and_lock_variants_are_preserved_but_later_changes_abort() {
    let directory = fixture();
    let root = directory.path();
    std::fs::write(root.join("README.md"), "unrelated user edits\n").unwrap();
    let original_lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    std::fs::write(
        root.join("Cargo.lock"),
        format!("{original_lock}\n# operator variant\n"),
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "// existing draft\n").unwrap();
    let source = Source::pin(root, false, &[]).unwrap();
    source.verify(false).unwrap();
    std::fs::write(root.join("README.md"), "more unrelated user edits\n").unwrap();
    assert!(source.verify(false).is_err());
    std::fs::write(root.join("README.md"), "unrelated user edits\n").unwrap();
    source.verify(false).unwrap();
    std::fs::write(root.join("src/lib.rs"), "// changed during collection\n").unwrap();
    assert!(source.verify(false).is_err());
    std::fs::write(root.join("src/lib.rs"), "// existing draft\n").unwrap();
    std::fs::write(root.join("Cargo.lock"), "changed during calibration\n").unwrap();
    assert!(source.verify(false).is_err());
}
#[test]
fn untracked_source_is_pinned_and_new_source_or_deletions_are_detected() {
    let directory = fixture();
    let draft = directory.path().join("src/new.rs");
    std::fs::write(&draft, "// existing untracked draft\n").unwrap();
    let source = Source::pin(directory.path(), false, &[]).unwrap();
    source.verify(false).unwrap();
    std::fs::remove_file(&draft).unwrap();
    assert!(source.verify(false).is_err());
    std::fs::write(&draft, "// existing untracked draft\n").unwrap();
    std::fs::write(directory.path().join("src/another.rs"), "// new draft\n").unwrap();
    assert!(source.verify(false).is_err());
}

#[test]
fn build_scripts_configuration_and_non_rust_includes_are_frozen() {
    let directory = fixture();
    let root = directory.path();
    std::fs::create_dir(root.join(".cargo")).unwrap();
    for (path, contents) in [
        ("build.rs", "fn main() {}"),
        ("included.bin", "input"),
        (".cargo/config.toml", "# configuration"),
    ] {
        std::fs::write(root.join(path), contents).unwrap();
    }
    let source = Source::pin(root, false, &[]).unwrap();
    for (path, contents) in [
        ("build.rs", "fn main() {}"),
        ("included.bin", "input"),
        (".cargo/config.toml", "# configuration"),
    ] {
        std::fs::write(root.join(path), format!("{contents}\n")).unwrap();
        assert!(source.verify(false).is_err(), "accepted changed {path}");
        std::fs::write(root.join(path), contents).unwrap();
        source.verify(false).unwrap();
    }
}
#[test]
fn sibling_path_dependency_source_changes_abort_verification() {
    let directory = fixture();
    let dependency = fixture();
    let manifest = directory.path().join("Cargo.toml");
    let mut text = std::fs::read_to_string(&manifest).unwrap();
    // Give the second fixture a different package identity.
    std::fs::write(
        dependency.path().join("Cargo.toml"),
        "[package]\nname=\"dependency\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
    )
    .unwrap();
    text.push_str(&format!(
        "\n[dependencies]\ndependency = {{ path = {:?} }}\n",
        dependency.path()
    ));
    std::fs::write(&manifest, text).unwrap();
    assert!(
        std::process::Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success()
    );
    let source = Source::pin(directory.path(), false, &[]).unwrap();
    source.verify(false).unwrap();
    std::fs::write(
        dependency.path().join("src/lib.rs"),
        "// changed external dependency",
    )
    .unwrap();
    assert!(source.verify(false).is_err());
}

#[test]
fn generated_table_and_calibration_artifacts_are_checked_outside_source_guard() {
    let directory = fixture();
    let root = directory.path();
    let manifest = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .unwrap()
        .replace("fixture", "candle-cache");
    std::fs::write(&manifest, text).unwrap();
    assert!(
        std::process::Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    std::fs::create_dir_all(root.join("src/spread_profiles")).unwrap();
    std::fs::create_dir_all(root.join("validation")).unwrap();
    std::fs::create_dir_all(root.join("target/nested/src")).unwrap();
    let source = Source::pin(root, false, &[]).unwrap();
    for path in [
        "src/spread_profiles/ctrader.rs",
        "src/spread_profiles/ctrader.rs.lock",
        "validation/report.json",
        "target/nested/src/generated.rs",
    ] {
        std::fs::write(root.join(path), "generated output").unwrap();
        source.verify(false).unwrap();
    }
}

#[test]
fn ancestor_configuration_creation_and_toolchain_edits_abort() {
    let directory = fixture();
    let root = directory.path();
    let source = Source::pin(root, false, &[]).unwrap();
    std::fs::write(
        root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel=\"invalid-channel\"\n",
    )
    .unwrap();
    assert!(source.verify(false).is_err());
}

#[test]
fn requested_reports_outside_validation_do_not_invalidate_their_own_source_pin() {
    let directory = fixture();
    let root = directory.path();
    let report = root.join("r.json");
    let status = root.join("s.json");
    let source = Source::pin(root, false, &[report.clone(), status.clone()]).unwrap();
    for path in [
        report.clone(),
        status,
        report.with_extension("json.lock"),
        report.with_extension("finish.lock"),
        report.with_extension("json.123.tmp"),
    ] {
        std::fs::write(path, "generated artifact").unwrap();
        source.verify(false).unwrap();
    }
    std::fs::write(root.join("unrelated.json"), "a real included build input").unwrap();
    assert!(source.verify(false).is_err());
}
