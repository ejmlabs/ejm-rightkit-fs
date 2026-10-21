//! M8-O4 (achieved durability).

use rightkit_fs::{
    atomic_write_reporting, AtomicWriteOptions, AtomicWriter, PublicationDurability, SyncLevel,
};

#[test]
fn a_durable_write_reports_the_flush_level_and_publication() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record");
    let report = atomic_write_reporting(&path, b"data", AtomicWriteOptions::default()).unwrap();
    assert!(report.sync_level >= Some(SyncLevel::Fsync));
    #[cfg(not(target_vendor = "apple"))]
    assert_eq!(report.sync_level, Some(SyncLevel::Full));
    #[cfg(unix)]
    {
        assert_eq!(report.publication, PublicationDurability::DirectoryFlushed);
        #[cfg(not(target_vendor = "apple"))]
        assert!(report.is_fully_durable());
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"data");
}

#[test]
fn a_non_durable_write_reports_nothing_flushed_and_is_never_fully_durable() {
    let dir = tempfile::tempdir().unwrap();
    let options = AtomicWriteOptions {
        durable: false,
        ..AtomicWriteOptions::default()
    };
    let report = atomic_write_reporting(&dir.path().join("r"), b"x", options).unwrap();
    assert_eq!(report.sync_level, None);
    assert_eq!(report.publication, PublicationDurability::None);
    assert!(!report.is_fully_durable());
}

#[test]
fn a_streaming_writer_reports_through_commit_reporting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stream");
    let mut writer = AtomicWriter::create_with(&path, AtomicWriteOptions::default()).unwrap();
    writer.write_all(b"streamed").unwrap();
    let report = writer.commit_reporting().unwrap();
    assert!(report.sync_level.is_some());
    assert_eq!(std::fs::read(&path).unwrap(), b"streamed");
}

#[test]
fn a_failed_write_returns_an_error_not_a_report_and_leaves_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a-directory");
    std::fs::create_dir(&target).unwrap();
    assert!(atomic_write_reporting(&target, b"x", AtomicWriteOptions::default()).is_err());
    assert!(target.is_dir());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(leftovers.len(), 1, "only the directory remains");
}
