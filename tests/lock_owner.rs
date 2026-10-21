//! Owner record shapes and `probe_owner`.
//!
//! The owner file is read by other builds. CutRight reads `pid`, `hostname` and an RFC 3339
//! `started`; 0.3.3 wrote `label` and `started_unix_ms` only. Both shapes must parse, and a
//! record written now must parse in CutRight's shape.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rightkit_fs::{probe_owner, FileLock, LockOptions, LockOwner};
use serde::Deserialize;

/// What CutRight's owner struct reads. Unknown fields are ignored, as serde does by default.
#[derive(Deserialize)]
struct CutRightOwner {
    pid: u32,
    hostname: String,
    started: String,
}

fn owner(started_unix_ms: u64) -> LockOwner {
    LockOwner {
        pid: 4242,
        hostname: "host-a".into(),
        label: "render".into(),
        started_unix_ms,
    }
}

#[test]
fn owner_round_trip_writes_both_start_fields() {
    let original = owner(1_577_836_800_123);
    let json = serde_json::to_value(&original).unwrap();
    assert_eq!(json["started_unix_ms"], 1_577_836_800_123u64);
    assert_eq!(json["started"], "2020-01-01T00:00:00Z");
    assert_eq!(json["label"], "render");
    let back: LockOwner = serde_json::from_value(json).unwrap();
    assert_eq!(back, original);
}

#[test]
fn reads_the_rfc3339_only_record_cutright_writes() {
    let text = r#"{"pid":7,"hostname":"h","started":"2020-01-01T00:00:00Z"}"#;
    let owner: LockOwner = serde_json::from_str(text).unwrap();
    assert_eq!(owner.pid, 7);
    assert_eq!(owner.hostname, "h");
    assert_eq!(owner.label, "", "a record without a label reads as empty");
    assert_eq!(owner.started_unix_ms, 1_577_836_800_000);
    assert_eq!(
        owner.started(),
        UNIX_EPOCH + Duration::from_secs(1_577_836_800)
    );
}

#[test]
fn reads_the_0_3_3_record_with_unix_millis() {
    let text = r#"{"pid":7,"hostname":"h","label":"x","started_unix_ms":1577836800500}"#;
    let owner: LockOwner = serde_json::from_str(text).unwrap();
    assert_eq!(owner.label, "x");
    assert_eq!(owner.started_unix_ms, 1_577_836_800_500);
    assert_eq!(
        owner.started(),
        UNIX_EPOCH + Duration::from_millis(1_577_836_800_500)
    );
}

#[test]
fn prefers_unix_millis_when_both_start_fields_are_present() {
    let text = r#"{"pid":7,"hostname":"h","label":"","started_unix_ms":1577836800500,"started":"2020-01-01T00:00:00Z"}"#;
    let owner: LockOwner = serde_json::from_str(text).unwrap();
    assert_eq!(owner.started_unix_ms, 1_577_836_800_500);
}

#[test]
fn rejects_records_without_a_usable_start_time() {
    let missing = r#"{"pid":7,"hostname":"h"}"#;
    assert!(serde_json::from_str::<LockOwner>(missing).is_err());
    let not_rfc3339 = r#"{"pid":7,"hostname":"h","started":"yesterday"}"#;
    assert!(serde_json::from_str::<LockOwner>(not_rfc3339).is_err());
}

#[test]
fn lock_file_owner_record_is_readable_in_cutright_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.lock");
    let lock = FileLock::acquire(&path, &LockOptions::default().labeled("build")).unwrap();

    let raw = std::fs::read(dir.path().join("p.lock.owner")).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert!(value.get("started_unix_ms").is_some(), "{value}");
    assert!(value.get("started").is_some(), "{value}");

    let cutright: CutRightOwner = serde_json::from_slice(&raw).unwrap();
    assert_eq!(cutright.pid, std::process::id());
    assert!(!cutright.hostname.is_empty());
    assert_eq!(cutright.started.len(), 20, "whole seconds, Z suffix");
    assert!(cutright.started.ends_with('Z'));
    drop(lock);
}

#[test]
fn probe_owner_reads_the_record_without_taking_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.lock");
    assert!(probe_owner(&path).is_none(), "no record before any holder");
    assert!(
        !path.exists() && !dir.path().join("q.lock.owner").exists(),
        "probing must not create files"
    );

    let before = SystemTime::now();
    let lock = FileLock::acquire(&path, &LockOptions::default().labeled("build")).unwrap();
    let probed = probe_owner(&path).expect("record written by the holder");
    assert_eq!(probed.pid, std::process::id());
    assert_eq!(probed.label, "build");
    assert_eq!(Some(probed.clone()), FileLock::current_owner(&path));
    let started = probed.started();
    assert!(started + Duration::from_secs(1) >= before && started <= SystemTime::now());

    assert!(
        FileLock::try_acquire(&path).is_err(),
        "probing must leave the lock held"
    );
    drop(lock);

    assert!(
        probe_owner(&path).is_none(),
        "record removed on clean release"
    );
    FileLock::try_acquire(&path).expect("probe took no lock, so the lock is free");
}
