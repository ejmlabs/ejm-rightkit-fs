//! `StateDir::create` and `child` publish the link of every directory THEY CREATE: the parent is
//! synced before the walk descends, in order, and a failed sync stops the walk. A directory that
//! already exists is not synced, however it got there, and a second identical call syncs nothing.
//! There is no record of earlier syncs and no walk over pre-existing directories. The order is
//! observed through the `state::testing` event log (the power-loss argument is "the link is
//! flushed before anything is written below it", so the order is what is asserted).
//!
//! The temp root exists before each test starts, so its ancestors are never created, and the logs
//! below are exact: they hold only what the call under test did.

#![cfg(unix)]

use rightkit_fs::state::testing::{fail_dir_sync, take_dir_events, without_full_sync, DirEvent};
use rightkit_fs::{FsError, StateDir, StateOptions, SyncPolicy};
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    let _ = take_dir_events();
    (dir, path)
}

fn ino(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().ino()
}

#[test]
fn create_syncs_the_parent_of_each_new_directory_before_descending() {
    let (_g, root) = tmp();
    let state = StateDir::create(&root.join("new/deep")).unwrap();
    assert_eq!(
        take_dir_events(),
        vec![
            DirEvent::Created("new".into()),
            DirEvent::Synced(ino(&root)),
            DirEvent::Created("deep".into()),
            DirEvent::Synced(ino(&root.join("new"))),
        ],
        "each new directory is published by a sync of its parent before the next is made"
    );
    // The pinned leaf is usable and its own write syncs the leaf, as before.
    state.write("record", b"x").unwrap();
    assert_eq!(
        take_dir_events(),
        vec![DirEvent::Synced(ino(&root.join("new/deep")))]
    );
}

/// There is no record and no walk over existing directories: a second identical call syncs no
/// ancestor, and a call that extends the path syncs only the part it creates.
#[test]
fn a_second_identical_create_syncs_nothing() {
    let (_g, root) = tmp();
    StateDir::create(&root.join("a/b")).unwrap();
    let _ = take_dir_events();
    StateDir::create(&root.join("a/b")).unwrap();
    assert!(
        take_dir_events().is_empty(),
        "every directory exists, so nothing is created and nothing is synced"
    );
    StateDir::create(&root.join("a/b/c")).unwrap();
    assert_eq!(
        take_dir_events(),
        vec![
            DirEvent::Created("c".into()),
            DirEvent::Synced(ino(&root.join("a/b")))
        ],
        "only the created tail is synced"
    );
    StateDir::open(&root.join("a/b/c")).unwrap();
    assert!(take_dir_events().is_empty(), "open never creates");
}

/// The directory below was made by a different process (a real `mkdir`), as a crashed earlier run
/// would leave it. It is found, not created, so it is not synced: the residual the `atomic` module
/// docs state. Only the leaf this call creates is synced.
#[test]
fn a_directory_made_by_another_process_is_found_not_synced() {
    let (_g, root) = tmp();
    let status = std::process::Command::new("mkdir")
        .arg(root.join("other"))
        .status()
        .unwrap();
    assert!(status.success());
    let _ = take_dir_events();
    let state = StateDir::create(&root.join("other/leaf")).unwrap();
    assert_eq!(
        take_dir_events(),
        vec![
            DirEvent::Created("leaf".into()),
            DirEvent::Synced(ino(&root.join("other"))),
        ],
        "`other` is left alone; `leaf` is created and published through its parent"
    );
    state.write("record", b"x").unwrap();
}

/// A directory moved into an existing one is, to the next call, an existing directory like any
/// other: found, not synced.
#[test]
fn a_directory_moved_into_place_is_found_not_synced() {
    let (_g, root) = tmp();
    StateDir::create(&root.join("a/dir")).unwrap();
    std::fs::create_dir(root.join("b")).unwrap();
    std::fs::rename(root.join("a/dir"), root.join("b/dir")).unwrap();
    let _ = take_dir_events();
    StateDir::create(&root.join("b/dir")).unwrap();
    assert!(
        take_dir_events().is_empty(),
        "nothing is created, so nothing is synced"
    );
}

/// `child` has the same rule: an existing child is returned as it is, every time.
#[test]
fn a_child_made_by_another_process_is_found_not_synced() {
    let (_g, root) = tmp();
    let state = StateDir::create(&root.join("s")).unwrap();
    let status = std::process::Command::new("mkdir")
        .arg(root.join("s/found"))
        .status()
        .unwrap();
    assert!(status.success());
    let _ = take_dir_events();
    state.child("found").unwrap();
    state.child("found").unwrap();
    assert!(take_dir_events().is_empty());
}

/// The sync strength applies to the syncs a call makes, and a call that creates nothing makes
/// none: under `Strict` with the strongest flush unavailable, creating a new directory fails,
/// and finding an existing one succeeds.
#[test]
fn strict_applies_to_the_syncs_a_call_makes_and_a_call_that_creates_nothing_makes_none() {
    let (_g, root) = tmp();
    let strict = StateOptions {
        sync: SyncPolicy::Strict,
        ..StateOptions::default()
    };
    let _no_full_sync = without_full_sync();
    StateDir::create(&root.join("s")).expect("the fallback policy settles for a weaker flush");
    let error = StateDir::create_with(&root.join("t"), &strict)
        .map(|_| ())
        .expect_err("a strict request needs the strongest flush for the link it creates");
    assert!(matches!(error, FsError::DirSyncFailed { .. }), "{error:?}");
    let _ = take_dir_events();
    StateDir::create_with(&root.join("s"), &strict)
        .expect("an existing directory needs no sync, so there is nothing to fail");
    assert!(take_dir_events().is_empty());
}

#[test]
fn a_failed_ancestor_sync_stops_the_walk_with_a_typed_error() {
    let (_g, root) = tmp();
    let guard = fail_dir_sync(ErrorKind::Other);
    let error = StateDir::create(&root.join("new/deep"))
        .map(|_| ())
        .expect_err("a failed directory sync must propagate");
    drop(guard);
    assert!(
        matches!(&error, FsError::DirSyncFailed { path, .. } if path == &root.join("new")),
        "{error:?}"
    );
    assert_eq!(
        take_dir_events(),
        vec![
            DirEvent::Created("new".into()),
            DirEvent::Synced(ino(&root))
        ],
        "the walk did not descend past the unpublished directory"
    );
    assert!(
        !root.join("new/deep").exists(),
        "no directory was made below an unpublished one"
    );
}

#[test]
fn a_child_created_through_the_handle_is_published_through_its_parent() {
    let (_g, root) = tmp();
    let state = StateDir::create(&root.join("s")).unwrap();
    let _ = take_dir_events();
    state.child("kid").unwrap();
    assert_eq!(
        take_dir_events(),
        vec![
            DirEvent::Created("kid".into()),
            DirEvent::Synced(ino(&root.join("s")))
        ]
    );
}

/// The residual, asserted: `new` was created by the failed call and its link in `root` was never
/// synced. The retry finds `new` as an existing directory and does not sync it; it creates and
/// publishes only `deep`.
#[test]
fn a_retry_after_a_failed_sync_syncs_only_what_it_creates() {
    let (_g, root) = tmp();
    let guard = fail_dir_sync(ErrorKind::Other);
    StateDir::create(&root.join("new/deep"))
        .map(|_| ())
        .expect_err("the first sync fails");
    drop(guard);
    let _ = take_dir_events();
    let state = StateDir::create(&root.join("new/deep")).unwrap();
    assert_eq!(
        take_dir_events(),
        vec![
            DirEvent::Created("deep".into()),
            DirEvent::Synced(ino(&root.join("new"))),
        ],
        "`new` is found and left alone (the residual); `deep` is created and published"
    );
    state.write("record", b"x").unwrap();
}
