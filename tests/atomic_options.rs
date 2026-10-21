//! Atomic write options: defaults stay unchanged, `create_parents`, the replace modes, and the
//! typed error for a replace that succeeded when the directory `fsync` failed.

use std::time::Duration;

use rightkit_fs::{
    atomic_write_with_options, AtomicOptions, AtomicWriteOptions, ReplaceMode, RetryPolicy,
};
#[cfg(unix)]
use rightkit_fs::{AtomicWriter, FsError};

#[test]
fn defaults_match_the_original_rename_behaviour() {
    let options = AtomicWriteOptions::default();
    assert_eq!(options.replace, ReplaceMode::Rename);
    assert_eq!(options.retry_sharing_violation, None);
    assert!(options.durable);
    assert!(!options.private);
    assert!(!options.create_parents);

    let converted: AtomicWriteOptions = AtomicOptions::default().into();
    assert_eq!(converted.replace, ReplaceMode::Rename);
    assert_eq!(converted.retry_sharing_violation, None);
    assert_eq!(converted.durable, options.durable);
}

#[test]
fn create_parents_creates_missing_directories_only_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a").join("b").join("state.json");

    let refused = atomic_write_with_options(&target, AtomicWriteOptions::default(), |f| {
        std::io::Write::write_all(f, b"x")
    });
    assert!(refused.is_err(), "parents are not created by default");

    let options = AtomicWriteOptions {
        create_parents: true,
        ..AtomicWriteOptions::default()
    };
    atomic_write_with_options(&target, options, |f| std::io::Write::write_all(f, b"{}")).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"{}");
}

#[test]
fn move_write_through_mode_replaces_existing_and_creates_missing_targets() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("settings.json");
    let options = AtomicWriteOptions {
        replace: ReplaceMode::MoveWriteThrough,
        ..AtomicWriteOptions::default()
    };

    atomic_write_with_options(&target, options, |f| std::io::Write::write_all(f, b"first"))
        .expect("missing target is created");
    assert_eq!(std::fs::read(&target).unwrap(), b"first");

    atomic_write_with_options(&target, options, |f| {
        std::io::Write::write_all(f, b"second")
    })
    .expect("existing target is replaced");
    assert_eq!(std::fs::read(&target).unwrap(), b"second");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(rightkit_fs::atomic::TEMP_MARKER))
        .collect();
    assert!(leftovers.is_empty(), "no temp file left: {leftovers:?}");
}

#[test]
fn retry_policy_is_inert_where_no_sharing_violation_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("retry.json");
    let options = AtomicWriteOptions {
        retry_sharing_violation: Some(RetryPolicy::new(3, Duration::from_millis(1))),
        ..AtomicWriteOptions::default()
    };
    atomic_write_with_options(&target, options, |f| std::io::Write::write_all(f, b"ok")).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"ok");
}

#[cfg(unix)]
#[test]
fn directory_sync_failure_after_replace_is_typed_and_keeps_new_content() {
    if unsafe { libc::geteuid() } == 0 {
        // Root reads any directory, so the failure below cannot be produced.
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("state.json");
    std::fs::write(&target, b"old").unwrap();

    let mut writer = AtomicWriter::create_with(&target, AtomicWriteOptions::default()).unwrap();
    writer.write_all(b"new").unwrap();
    // Write and search stay allowed, so the temp file and the rename still succeed, but the
    // directory can no longer be opened for the `fsync` that follows.
    set_mode(dir.path(), 0o300);
    let result = writer.commit();
    set_mode(dir.path(), 0o700);

    let error = result.expect_err("directory sync must not be swallowed");
    assert!(error.target_replaced(), "{error}");
    assert!(matches!(error, FsError::ReplacedButDirSyncFailed { .. }));
    assert_eq!(error.kind(), Some(std::io::ErrorKind::PermissionDenied));
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"new",
        "replace did happen"
    );
}

#[cfg(unix)]
#[test]
fn directory_sync_is_skipped_when_not_durable() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("cache.bin");
    let options = AtomicWriteOptions {
        durable: false,
        ..AtomicWriteOptions::default()
    };
    let mut writer = AtomicWriter::create_with(&target, options).unwrap();
    writer.write_all(b"cache").unwrap();
    set_mode(dir.path(), 0o300);
    let result = writer.commit();
    set_mode(dir.path(), 0o700);
    result.expect("a non-durable write does not open the directory");
    assert_eq!(std::fs::read(&target).unwrap(), b"cache");
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}
