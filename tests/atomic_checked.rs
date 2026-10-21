//! `AtomicWriter::commit_checked`: a caller's check runs on the flushed temp file before
//! publication. A passing check publishes exactly the checked content; a failing check leaves the
//! destination untouched and removes the temp file.

use std::path::Path;

use rightkit_fs::{AtomicWriteOptions, AtomicWriter, FsError};

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_passing_check_sees_the_flushed_content_and_publishes_it() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("config.json");
    std::fs::write(&target, b"old").unwrap();

    let mut writer = AtomicWriter::create_with(&target, AtomicWriteOptions::default()).unwrap();
    writer.write_all(b"new content").unwrap();
    let mut seen = None;
    writer
        .commit_checked(|temp| {
            seen = Some(std::fs::read(temp)?);
            Ok(())
        })
        .expect("a passing check lets the write publish");

    assert_eq!(seen.as_deref(), Some(&b"new content"[..]));
    assert_eq!(std::fs::read(&target).unwrap(), b"new content");
    assert_eq!(names(dir.path()), vec!["config.json".to_string()]);
}

#[test]
fn a_failing_check_leaves_the_old_file_and_removes_the_temp() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("config.json");
    std::fs::write(&target, b"old").unwrap();

    let mut writer = AtomicWriter::create_with(&target, AtomicWriteOptions::default()).unwrap();
    writer.write_all(b"rejected").unwrap();
    let error = writer
        .commit_checked(|_| Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "no")))
        .unwrap_err();

    match &error {
        FsError::Io { path, source } => {
            assert_ne!(path, &target, "the error names the temp file");
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidData);
        }
        other => panic!("expected FsError::Io, got {other:?}"),
    }
    assert!(!error.target_replaced());
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert_eq!(names(dir.path()), vec!["config.json".to_string()]);
}

#[test]
fn a_failing_check_on_a_new_destination_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("fresh.json");

    let mut writer = AtomicWriter::create_with(&target, AtomicWriteOptions::default()).unwrap();
    writer.write_all(b"x").unwrap();
    let result = writer.commit_checked(|_| Err(std::io::Error::other("refused")));

    assert!(result.is_err());
    assert!(!target.exists());
    assert!(names(dir.path()).is_empty(), "no temp file is left behind");
}

#[test]
fn plain_commit_still_publishes_without_a_check() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("plain.json");

    let mut writer = AtomicWriter::create_with(&target, AtomicWriteOptions::default()).unwrap();
    writer.write_all(b"{}").unwrap();
    writer.commit().unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), b"{}");
}
