//! `AtomicWriter::commit_checked` syncs the temp file before the check in every mode, and syncs
//! the directory only when `durable`.

use rightkit_fs::atomic::testing::{take_steps, Step};
use rightkit_fs::{AtomicWriteOptions, AtomicWriter};

fn non_durable() -> AtomicWriteOptions {
    AtomicWriteOptions {
        durable: false,
        ..AtomicWriteOptions::default()
    }
}

#[test]
fn a_non_durable_checked_write_syncs_the_file_before_the_check_and_never_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("config.json");
    let _ = take_steps();

    let mut writer = AtomicWriter::create_with(&target, non_durable()).unwrap();
    writer.write_all(b"checked bytes").unwrap();
    let mut before_check = None;
    let mut seen = None;
    writer
        .commit_checked(|temp| {
            before_check = Some(take_steps());
            seen = Some(std::fs::read(temp)?);
            Ok(())
        })
        .unwrap();

    assert_eq!(before_check, Some(vec![Step::TempFlushed]));
    assert_eq!(seen.as_deref(), Some(&b"checked bytes"[..]));
    let after = take_steps();
    assert_eq!(
        after,
        vec![Step::Replace],
        "no directory sync when not durable"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"checked bytes");
}

#[test]
fn a_non_durable_plain_commit_still_does_no_sync() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("plain.json");
    let _ = take_steps();

    let mut writer = AtomicWriter::create_with(&target, non_durable()).unwrap();
    writer.write_all(b"x").unwrap();
    writer.commit().unwrap();

    assert_eq!(take_steps(), vec![Step::Replace]);
}

#[cfg(unix)]
#[test]
fn a_durable_checked_write_syncs_the_file_before_the_check_and_the_directory_after() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("durable.json");
    let _ = take_steps();

    let mut writer = AtomicWriter::create_with(&target, AtomicWriteOptions::default()).unwrap();
    writer.write_all(b"d").unwrap();
    let mut before_check = None;
    writer
        .commit_checked(|_| {
            before_check = Some(take_steps());
            Ok(())
        })
        .unwrap();

    assert_eq!(before_check, Some(vec![Step::TempFlushed]));
    assert_eq!(
        take_steps(),
        vec![Step::Replace, Step::DirSynced(dir.path().to_path_buf())]
    );
}
