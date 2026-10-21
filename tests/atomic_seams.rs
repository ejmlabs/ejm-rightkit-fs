//! The release-safe seams (`atomic::seams`, feature `test-seams`): fail a write after N bytes and
//! count file and directory syncs. This target needs only `test-seams`, so it compiles and runs in
//! a release-profile test run, where feature `testing` does not.

use std::io::Write;
use std::path::PathBuf;

use rightkit_fs::atomic::seams::{
    fail_write_after, reset_sync_counts, sync_counts, take_sync_counts, SyncCounts,
};
use rightkit_fs::{
    atomic_write_with_options, publish_file_first_free, AtomicWriteOptions, AtomicWriter, FsError,
};

fn durable() -> AtomicWriteOptions {
    AtomicWriteOptions {
        durable: true,
        ..AtomicWriteOptions::default()
    }
}

fn plain() -> AtomicWriteOptions {
    AtomicWriteOptions {
        durable: false,
        ..AtomicWriteOptions::default()
    }
}

/// Directory syncs a durable publish makes: one `fsync` of the destination directory on Unix, none
/// on Windows (no directory flush there).
const DIR_SYNCS: u64 = if cfg!(unix) { 1 } else { 0 };

#[test]
fn a_write_fails_after_n_bytes_leaving_the_destination_and_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("out.bin");
    std::fs::write(&target, b"old").unwrap();

    let guard = fail_write_after(4, std::io::ErrorKind::StorageFull);
    let mut writer = AtomicWriter::create_with(&target, plain()).unwrap();
    let temp = writer.temp_path().to_path_buf();
    let error = writer.write_all(b"0123456789").unwrap_err();
    assert_eq!(
        error.kind(),
        Some(std::io::ErrorKind::StorageFull),
        "{error:?}"
    );
    drop(guard);
    assert_eq!(
        std::fs::read(&temp).unwrap(),
        b"0123",
        "the bytes up to the limit were written, as a full disk would"
    );
    drop(writer);
    assert!(!temp.exists(), "the dropped writer removes its temp");
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
}

#[test]
fn the_budget_is_cumulative_across_writes_and_writers() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = fail_write_after(10, std::io::ErrorKind::StorageFull);
    let mut first = AtomicWriter::create_with(&dir.path().join("a"), plain()).unwrap();
    first.write_all(b"123456").unwrap();
    first.commit().unwrap();
    let mut second = AtomicWriter::create_with(&dir.path().join("b"), plain()).unwrap();
    assert!(second.write_all(b"123456").is_err(), "4 bytes were left");
    let mut third = AtomicWriter::create_with(&dir.path().join("c"), plain()).unwrap();
    assert!(third.write_all(b"1").is_err(), "the budget is spent");
    assert!(
        third.write_all(b"").is_ok(),
        "an empty write needs no bytes"
    );
}

#[test]
fn the_writer_adapter_goes_through_the_same_seam() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("out.bin");
    let _guard = fail_write_after(3, std::io::ErrorKind::StorageFull);
    let mut writer = AtomicWriter::create_with(&target, plain()).unwrap();
    let mut source: &[u8] = b"abcdefgh";
    let error = std::io::copy(&mut source, &mut writer.writer()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::StorageFull);
    assert_eq!(std::fs::read(writer.temp_path()).unwrap(), b"abc");
}

#[test]
fn nothing_fails_once_the_guard_drops_or_on_another_thread() {
    let dir = tempfile::tempdir().unwrap();
    let guard = fail_write_after(0, std::io::ErrorKind::StorageFull);
    let elsewhere = dir.path().join("elsewhere");
    let path = elsewhere.clone();
    std::thread::spawn(move || {
        atomic_write_with_options(&path, plain(), |f| f.write_all(b"fine")).unwrap();
    })
    .join()
    .unwrap();
    assert_eq!(std::fs::read(&elsewhere).unwrap(), b"fine");
    drop(guard);
    let mut writer = AtomicWriter::create_with(&dir.path().join("here"), plain()).unwrap();
    writer.write_all(b"fine").unwrap();
    writer.commit().unwrap();
}

#[test]
fn a_durable_write_syncs_the_file_once_and_the_directory_once() {
    let dir = tempfile::tempdir().unwrap();
    reset_sync_counts();
    atomic_write_with_options(&dir.path().join("a"), durable(), |f| f.write_all(b"x")).unwrap();
    assert_eq!(
        take_sync_counts(),
        SyncCounts {
            file_syncs: 1,
            dir_syncs: DIR_SYNCS
        }
    );
    assert_eq!(
        sync_counts(),
        SyncCounts::default(),
        "take zeroes the counts"
    );
}

#[test]
fn a_non_durable_write_syncs_nothing_and_a_checked_one_syncs_only_the_file() {
    let dir = tempfile::tempdir().unwrap();
    reset_sync_counts();
    atomic_write_with_options(&dir.path().join("a"), plain(), |f| f.write_all(b"x")).unwrap();
    assert_eq!(take_sync_counts(), SyncCounts::default());

    let mut writer = AtomicWriter::create_with(&dir.path().join("b"), plain()).unwrap();
    writer.write_all(b"x").unwrap();
    writer.commit_checked(|_| Ok(())).unwrap();
    assert_eq!(
        take_sync_counts(),
        SyncCounts {
            file_syncs: 1,
            dir_syncs: 0
        }
    );
}

#[test]
fn the_candidate_list_syncs_the_source_once_however_many_names_are_taken() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("work.tmp");
    std::fs::write(&from, b"finished").unwrap();
    let candidates: Vec<PathBuf> = ["a", "b", "c"].iter().map(|n| dir.path().join(n)).collect();
    std::fs::write(&candidates[0], b"x").unwrap();
    std::fs::write(&candidates[1], b"y").unwrap();
    reset_sync_counts();
    let published = publish_file_first_free(&from, &candidates, durable()).unwrap();
    assert_eq!(published, candidates[2]);
    assert_eq!(
        take_sync_counts(),
        SyncCounts {
            file_syncs: 1,
            dir_syncs: DIR_SYNCS
        },
        "one source sync and one directory sync (source and destination share a directory)"
    );
}

#[test]
fn a_refused_exclusive_publish_syncs_no_directory() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("taken");
    std::fs::write(&target, b"winner").unwrap();
    reset_sync_counts();
    let options = AtomicWriteOptions {
        replace: rightkit_fs::ReplaceMode::NoReplace,
        ..durable()
    };
    let error = atomic_write_with_options(&target, options, |f| f.write_all(b"x")).unwrap_err();
    assert!(matches!(error, FsError::AlreadyExists(_)), "{error:?}");
    assert_eq!(
        take_sync_counts(),
        SyncCounts {
            file_syncs: 1,
            dir_syncs: 0
        }
    );
}
