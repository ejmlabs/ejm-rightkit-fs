//! M8-O3: `append_record`.

use rightkit_fs::{append_record, AppendOptions, FsError};

#[test]
fn creates_then_appends_in_order_and_reports_the_sync_level() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    let first = append_record(&path, b"one\n", AppendOptions::default()).unwrap();
    assert!(
        first.is_some(),
        "a durable append reports the level reached"
    );
    append_record(&path, b"two\n", AppendOptions::default()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"one\ntwo\n");
}

#[test]
fn non_durable_append_reports_no_sync_level() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    let options = AppendOptions::default().durable(false);
    assert_eq!(append_record(&path, b"x", options).unwrap(), None);
    assert_eq!(std::fs::read(&path).unwrap(), b"x");
}

#[cfg(unix)]
#[test]
fn private_creates_0600_and_never_changes_an_existing_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh");
    append_record(&fresh, b"a\n", AppendOptions::private()).unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&fresh), 0o600);
    append_record(&fresh, b"b\n", AppendOptions::private()).unwrap();
    assert_eq!(std::fs::read(&fresh).unwrap(), b"a\nb\n");

    let existing = dir.path().join("existing");
    std::fs::write(&existing, b"x").unwrap();
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).unwrap();
    append_record(&existing, b"y", AppendOptions::private()).unwrap();
    assert_eq!(std::fs::read(&existing).unwrap(), b"xy");
    assert_eq!(mode(&existing), 0o644, "an existing file keeps its mode");
}

#[cfg(unix)]
#[test]
fn a_symlink_at_the_path_is_refused_and_its_target_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::write(&target, b"keep").unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let error = append_record(&link, b"bad", AppendOptions::default()).unwrap_err();
    assert!(matches!(error, FsError::Symlink(_)), "{error:?}");
    assert_eq!(std::fs::read(&target).unwrap(), b"keep");
}

#[test]
fn a_directory_a_generated_temp_name_and_a_missing_parent_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let error = append_record(dir.path(), b"x", AppendOptions::default()).unwrap_err();
    assert!(
        matches!(error, FsError::NotAFile(_) | FsError::BadName(_)),
        "{error:?}"
    );
    let temp_shaped = dir.path().join(".ledger.rktmp-123-1-0");
    let error = append_record(&temp_shaped, b"x", AppendOptions::default()).unwrap_err();
    assert!(matches!(error, FsError::BadName(_)), "{error:?}");
    assert!(!temp_shaped.exists());
    let orphan = dir.path().join("no-such-dir").join("f");
    assert!(append_record(&orphan, b"x", AppendOptions::default())
        .unwrap_err()
        .is_not_found());
}

#[test]
fn concurrent_appenders_never_split_a_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared");
    let options = AppendOptions::default().durable(false);
    std::thread::scope(|scope| {
        for t in 0..8 {
            let path = &path;
            scope.spawn(move || {
                for i in 0..100 {
                    let record = format!("t{t}-i{i}-{}\n", "x".repeat(64));
                    append_record(path, record.as_bytes(), options).unwrap();
                }
            });
        }
    });
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 800);
    for line in text.lines() {
        assert!(
            line.starts_with('t') && line.ends_with(&"x".repeat(64)),
            "torn record: {line:?}"
        );
    }
}
