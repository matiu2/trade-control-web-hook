use super::*;

#[test]
fn resume_cannot_replace_an_unrelated_existing_output() {
    crate::ctrader::test_logging();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("a.json");
    let target = directory.path().join("b.json");
    std::fs::write(&source, "source checkpoint").unwrap();
    std::fs::write(&target, "unrelated report").unwrap();
    assert!(Destination::acquire(&target, Some(&source)).is_err());
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "unrelated report"
    );
    assert!(Destination::acquire(&target, None).is_err());
    assert!(Destination::acquire(&source, Some(&source)).is_ok());
}

#[tokio::test]
async fn bad_resume_leaves_new_output_absent_and_existing_source_untouched() {
    crate::ctrader::test_logging();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("a.json");
    let target = directory.path().join("b.json");
    std::fs::write(&source, "invalid JSON checkpoint").unwrap();
    let destination = Destination::acquire(&target, Some(&source)).unwrap();
    assert!(destination.read_resume().await.is_err());
    assert!(!target.exists());
    assert_eq!(
        std::fs::read_to_string(&source).unwrap(),
        "invalid JSON checkpoint"
    );
}

#[test]
fn canonical_aliases_share_a_lock_and_release_on_drop() {
    crate::ctrader::test_logging();
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("report.json");
    let alias = directory.path().join(".").join("report.json");
    let first = Destination::acquire(&target, None).unwrap();
    assert!(Destination::acquire(&alias, None).is_err());
    drop(first);
    assert!(Destination::acquire(&alias, None).is_ok());
}

#[test]
fn source_is_locked_while_resuming_into_a_new_output() {
    crate::ctrader::test_logging();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("a.json");
    let target = directory.path().join("b.json");
    std::fs::write(&source, "source checkpoint").unwrap();
    let first = Destination::acquire(&target, Some(&source)).unwrap();
    assert!(Destination::acquire(&source, Some(&source)).is_err());
    drop(first);
    assert!(Destination::acquire(&source, Some(&source)).is_ok());
}

#[cfg(unix)]
#[test]
fn directories_and_symlink_reports_are_refused() {
    use std::os::unix::fs::symlink;
    crate::ctrader::test_logging();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.json");
    let link = directory.path().join("link.json");
    let folder = directory.path().join("folder.json");
    std::fs::write(&source, "checkpoint").unwrap();
    std::fs::create_dir(&folder).unwrap();
    symlink(&source, &link).unwrap();
    assert!(Destination::acquire(&folder, Some(&source)).is_err());
    assert!(Destination::acquire(&source, Some(&link)).is_err());
    assert!(Destination::acquire(&link, Some(&source)).is_err());
}

#[tokio::test]
async fn bad_resume_at_the_output_path_is_not_replaced() {
    crate::ctrader::test_logging();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("report.json");
    std::fs::write(&source, "invalid existing report").unwrap();
    let destination = Destination::acquire(&source, Some(&source)).unwrap();
    assert!(destination.read_resume().await.is_err());
    assert_eq!(
        std::fs::read_to_string(&source).unwrap(),
        "invalid existing report"
    );
}

#[cfg(unix)]
#[test]
fn dropping_owner_unlocks_despite_an_inherited_descriptor_without_unlocking_the_next_owner() {
    crate::ctrader::test_logging();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("report.json");
    let first = Destination::acquire(&path, None).unwrap();
    let inherited = first._locks[0].file.try_clone().unwrap();
    drop(first);
    let second = Destination::acquire(&path, None).unwrap();
    assert!(Destination::acquire(&path, None).is_err());
    drop(inherited);
    assert!(Destination::acquire(&path, None).is_err());
    drop(second);
    assert!(Destination::acquire(&path, None).is_ok());
    assert!(!path.exists());
}

#[test]
fn failed_partial_acquisition_releases_earlier_locks_without_changing_checkpoints() {
    crate::ctrader::test_logging();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("a.json");
    let target = directory.path().join("z.json");
    std::fs::write(&source, b"original checkpoint").unwrap();
    let owner = Destination::acquire(&target, None).unwrap();
    assert!(Destination::acquire(&target, Some(&source)).is_err());
    let source_owner = Destination::acquire(&source, Some(&source)).unwrap();
    assert!(Destination::acquire(&target, None).is_err());
    assert_eq!(std::fs::read(&source).unwrap(), b"original checkpoint");
    assert!(!target.exists());
    drop(source_owner);
    drop(owner);
    assert!(Destination::acquire(&target, Some(&source)).is_ok());
}

#[cfg(unix)]
#[test]
fn failed_post_lock_validation_unlocks_inherited_descriptors_without_replacing_the_racing_report() {
    crate::ctrader::test_logging();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("report.json");
    let (output, resume) = validate_paths(&path, None).unwrap();
    let owned = lock(&output).unwrap();
    let inherited = owned.file.try_clone().unwrap();
    std::fs::write(&path, b"concurrent report").unwrap();
    assert!(Destination::finish_acquisition(output, resume, vec![owned]).is_err());
    let next = Destination::acquire(&path, Some(&path)).unwrap();
    drop(inherited);
    assert!(Destination::acquire(&path, Some(&path)).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"concurrent report");
    drop(next);
    assert!(Destination::acquire(&path, Some(&path)).is_ok());
}
