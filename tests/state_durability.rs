//! StateDir durability, naming, listing and options (ledger row 111). Fault injection uses the
//! `testing` feature's `state::testing` seam.

use rightkit_fs::{FsError, StateDir};
use std::path::PathBuf;

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

#[cfg(any(unix, windows))]
mod direct_empty {
    use super::*;
    use rightkit_fs::handle::{link_count, ReadOptions};
    use rightkit_fs::{FileKey, StateOptions};
    use std::fs::{File, TryLockError};
    use std::path::Path;
    use std::sync::Barrier;

    #[test]
    fn racing_creators_keep_one_inode_and_all_losers_can_open_it() {
        let (_guard, root) = tmp();
        let state = StateDir::create(&root.join("state")).unwrap();
        let start = Barrier::new(8);
        let attempts = std::thread::scope(|scope| {
            let mut clients = Vec::new();
            for _ in 0..8 {
                let state = &state;
                let start = &start;
                clients.push(scope.spawn(move || {
                    start.wait();
                    state.create_new_empty("lock")
                }));
            }
            clients
                .into_iter()
                .map(|client| client.join().unwrap())
                .collect::<Vec<_>>()
        });
        let mut winners = Vec::new();
        for attempt in attempts {
            match attempt {
                Ok(file) => winners.push(file),
                Err(FsError::AlreadyExists(_)) => {}
                Err(error) => panic!("unexpected creation error: {error:?}"),
            }
        }
        assert_eq!(winners.len(), 1);
        let winner = winners.pop().unwrap();
        assert_eq!(winner.metadata().unwrap().len(), 0);
        assert_eq!(link_count(&winner).unwrap(), 1);
        assert!(rightkit_fs::private::is_owner_only_strict(&state.path().join("lock")).unwrap());
        let key = FileKey::of_file(&winner, Path::new("lock")).unwrap();
        let peers = (0..8)
            .map(|_| {
                let peer = state
                    .open_read(
                        "lock",
                        &ReadOptions::new()
                            .require_single_link(true)
                            .require_owner_only(true),
                    )
                    .unwrap();
                assert_eq!(FileKey::of_file(&peer, Path::new("lock")).unwrap(), key);
                peer
            })
            .collect::<Vec<_>>();
        winner.try_lock().unwrap();
        for peer in &peers {
            assert!(matches!(peer.try_lock(), Err(TryLockError::WouldBlock)));
        }
        File::unlock(&winner).unwrap();
        peers[0].try_lock().unwrap();
        File::unlock(&peers[0]).unwrap();
    }

    #[test]
    fn a_replaced_path_cannot_redirect_direct_creation() {
        let (_guard, root) = tmp();
        let path = root.join("state");
        let moved = root.join("moved");
        let state = StateDir::create_with(
            &path,
            &StateOptions {
                allow_rename_while_open: true,
                ..StateOptions::default()
            },
        )
        .unwrap();
        std::fs::rename(&path, &moved).unwrap();
        std::fs::create_dir(&path).unwrap();
        let file = state.create_new_empty("lock").unwrap();
        assert_eq!(
            FileKey::of_file(&file, Path::new("lock")).unwrap(),
            FileKey::of_path(&moved.join("lock")).unwrap()
        );
        assert!(!path.join("lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn owner_only_read_checks_the_pinned_inode_and_preserves_plain_reads() {
        use std::os::unix::fs::PermissionsExt;

        let (_guard, root) = tmp();
        let path = root.join("state");
        let moved = root.join("moved");
        let state = StateDir::create(&path).unwrap();
        let original = state.create_new_empty("lock").unwrap();
        let key = FileKey::of_file(&original, Path::new("lock")).unwrap();
        std::fs::rename(&path, &moved).unwrap();
        let replacement = StateDir::create(&path).unwrap();
        replacement.create_new_empty("lock").unwrap();
        original
            .set_permissions(std::fs::Permissions::from_mode(0o644))
            .unwrap();

        let strict = ReadOptions::new()
            .require_single_link(true)
            .require_owner_only(true);
        assert!(matches!(
            state.open_read("lock", &strict),
            Err(FsError::TooPermissive(_))
        ));
        assert!(replacement.open_read("lock", &strict).is_ok());
        assert!(matches!(
            rightkit_fs::handle::open_read_nofollow(&moved.join("lock"), &strict),
            Err(FsError::TooPermissive(_))
        ));
        let plain = state.open_read("lock", &ReadOptions::new()).unwrap();
        assert_eq!(FileKey::of_file(&plain, Path::new("lock")).unwrap(), key);
        assert_eq!(
            plain.metadata().unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[cfg(windows)]
    #[test]
    fn inherited_privacy_is_refused_before_direct_creation() {
        for privacy in [
            rightkit_fs::Privacy::Inherit,
            rightkit_fs::Privacy::OwnerOnlyUnixInheritWindows,
        ] {
            let (_guard, root) = tmp();
            let state = StateDir::create_with(
                &root.join("state"),
                &StateOptions {
                    privacy,
                    ..StateOptions::default()
                },
            )
            .unwrap();
            assert!(matches!(
                state.create_new_empty("lock"),
                Err(FsError::Unavailable(_))
            ));
            assert!(state.list(8).unwrap().is_empty());
        }
    }

    #[test]
    fn existing_content_and_invalid_names_are_unchanged() {
        let (_guard, root) = tmp();
        let state = StateDir::create_with(
            &root.join("state"),
            &StateOptions {
                max_name_len: Some(8),
                ..StateOptions::default()
            },
        )
        .unwrap();
        state.write("keep", b"original").unwrap();
        let key = FileKey::of_path(&state.path().join("keep")).unwrap();
        assert!(matches!(
            state.create_new_empty("keep"),
            Err(FsError::AlreadyExists(_))
        ));
        assert_eq!(state.read("keep", 32).unwrap(), b"original");
        assert_eq!(FileKey::of_path(&state.path().join("keep")).unwrap(), key);
        for name in ["", ".", "..", "../escape", "nested/x", "123456789"] {
            assert!(matches!(
                state.create_new_empty(name),
                Err(FsError::BadName(_))
            ));
        }
        let ordinary = StateDir::create(&root.join("ordinary")).unwrap();
        assert!(matches!(
            ordinary.create_new_empty(".lock.rktmp-1-2-3"),
            Err(FsError::BadName(_))
        ));
        assert!(ordinary.list(8).unwrap().is_empty());
        assert_eq!(state.list(8).unwrap(), vec!["keep"]);
    }

    #[cfg(unix)]
    #[test]
    fn planted_links_and_directories_are_never_followed_or_replaced() {
        let (_guard, root) = tmp();
        let state = StateDir::create(&root.join("state")).unwrap();
        let outside = root.join("outside");
        std::fs::write(&outside, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, state.path().join("link")).unwrap();
        std::fs::create_dir(state.path().join("dir")).unwrap();
        assert!(state.create_new_empty("link").is_err());
        assert!(state.create_new_empty("dir").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
        assert!(std::fs::symlink_metadata(state.path().join("link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(state.path().join("dir").is_dir());
    }

    #[cfg(feature = "testing")]
    mod boundary {
        use super::*;
        use rightkit_fs::state::testing::on_empty_created;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        #[test]
        fn the_first_visible_name_has_one_link_and_can_be_locked_by_a_peer() {
            let (_guard, root) = tmp();
            let path = root.join("state");
            let state = StateDir::create(&path).unwrap();
            let observer = StateDir::open(&path).unwrap();
            let observed = Arc::new(AtomicBool::new(false));
            let seen = Arc::clone(&observed);
            let hook = on_empty_created(move |created| {
                assert_eq!(observer.list(8).unwrap(), vec!["lock"]);
                assert_eq!(link_count(created).unwrap(), 1);
                let peer = observer
                    .open_read(
                        "lock",
                        &ReadOptions::new()
                            .require_single_link(true)
                            .require_owner_only(true),
                    )
                    .unwrap();
                assert_eq!(
                    FileKey::of_file(created, Path::new("lock")).unwrap(),
                    FileKey::of_file(&peer, Path::new("lock")).unwrap()
                );
                created.try_lock().unwrap();
                assert!(matches!(peer.try_lock(), Err(TryLockError::WouldBlock)));
                File::unlock(created).unwrap();
                seen.store(true, Ordering::SeqCst);
                Ok(())
            });
            let file = state.create_new_empty("lock").unwrap();
            drop(hook);
            assert!(observed.load(Ordering::SeqCst));
            assert_eq!(link_count(&file).unwrap(), 1);
        }

        #[test]
        fn a_real_permission_failure_keeps_the_inode_and_blocks_private_reopen() {
            let (_guard, root) = tmp();
            let state = StateDir::create(&root.join("state")).unwrap();
            let key = Arc::new(Mutex::new(None));
            let captured = Arc::clone(&key);
            #[cfg(windows)]
            let shown = state.path().join("lock");
            let hook = on_empty_created(move |created| {
                *captured.lock().unwrap() =
                    Some(FileKey::of_file(created, Path::new("lock")).unwrap());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    created
                        .set_permissions(std::fs::Permissions::from_mode(0o644))
                        .unwrap();
                }
                #[cfg(windows)]
                {
                    let status = std::process::Command::new("icacls")
                        .arg(&shown)
                        .args(["/grant", "*S-1-1-0:(R)"])
                        .stdout(std::process::Stdio::null())
                        .status()
                        .unwrap();
                    assert!(status.success(), "icacls could not widen the new lock");
                }
                Ok(())
            });
            assert!(matches!(
                state.create_new_empty("lock"),
                Err(FsError::TooPermissive(_))
            ));
            drop(hook);
            assert!(matches!(
                state.open_read(
                    "lock",
                    &ReadOptions::new()
                        .require_single_link(true)
                        .require_owner_only(true)
                ),
                Err(FsError::TooPermissive(_))
            ));
            let peer = state
                .open_read("lock", &ReadOptions::new().require_single_link(true))
                .unwrap();
            assert_eq!(
                Some(FileKey::of_file(&peer, Path::new("lock")).unwrap()),
                *key.lock().unwrap()
            );
            assert!(matches!(
                state.create_new_empty("lock"),
                Err(FsError::AlreadyExists(_))
            ));
            assert_eq!(
                Some(FileKey::of_file(&peer, Path::new("lock")).unwrap()),
                *key.lock().unwrap()
            );
        }

        #[cfg(unix)]
        #[test]
        fn a_second_link_at_the_boundary_is_rejected_without_removing_names() {
            let (_guard, root) = tmp();
            let state = StateDir::create(&root.join("state")).unwrap();
            let path = state.path().to_path_buf();
            let hook = on_empty_created(move |_| {
                std::fs::hard_link(path.join("lock"), path.join("alias")).unwrap();
                Ok(())
            });
            assert!(matches!(
                state.create_new_empty("lock"),
                Err(FsError::HardLinked(_))
            ));
            drop(hook);
            let mut names = state.list(8).unwrap();
            names.sort();
            assert_eq!(names, vec!["alias", "lock"]);
            assert!(matches!(
                state.open_read("lock", &ReadOptions::new().require_single_link(true)),
                Err(FsError::HardLinked(_))
            ));
        }
    }
}

// ------------------------------------------------------------ 1: directory sync propagates

#[cfg(all(unix, feature = "testing"))]
mod dir_sync {
    use super::*;
    use rightkit_fs::state::testing::{dir_sync_count, fail_dir_sync};
    use std::io::ErrorKind;

    #[test]
    fn write_reports_a_failed_directory_sync_and_the_content_is_in_place() {
        let (_g, dir) = tmp();
        let state = StateDir::create(&dir.join("s")).unwrap();
        let guard = fail_dir_sync(ErrorKind::Other);
        let err = state.write("a", b"new").expect_err("directory sync failed");
        drop(guard);
        assert!(
            matches!(err, FsError::ReplacedButDirSyncFailed { .. }),
            "{err:?}"
        );
        assert!(err.target_replaced());
        assert_eq!(state.read("a", 10).unwrap(), b"new");
    }

    #[test]
    fn write_new_reports_a_failed_directory_sync() {
        let (_g, dir) = tmp();
        let state = StateDir::create(&dir.join("s")).unwrap();
        let guard = fail_dir_sync(ErrorKind::Other);
        let err = state
            .write_new("a", b"new")
            .expect_err("directory sync failed");
        drop(guard);
        assert!(
            matches!(err, FsError::ReplacedButDirSyncFailed { .. }),
            "{err:?}"
        );
        assert_eq!(state.read("a", 10).unwrap(), b"new");
    }

    #[test]
    fn remove_reports_a_failed_directory_sync_and_the_entry_is_gone() {
        let (_g, dir) = tmp();
        let state = StateDir::create(&dir.join("s")).unwrap();
        state.write("a", b"x").unwrap();
        let guard = fail_dir_sync(ErrorKind::Other);
        let err = state.remove("a").expect_err("directory sync failed");
        drop(guard);
        assert!(matches!(err, FsError::DirSyncFailed { .. }), "{err:?}");
        assert!(!state.exists("a").unwrap());
        // Removing something that is not there changes nothing, so there is nothing to sync.
        let guard = fail_dir_sync(ErrorKind::Other);
        assert!(!state.remove("a").unwrap());
        drop(guard);
    }

    #[test]
    fn creating_a_child_reports_a_failed_directory_sync_and_a_retry_does_not_sync_it_again() {
        let (_g, dir) = tmp();
        let state = StateDir::create(&dir.join("s")).unwrap();
        let guard = fail_dir_sync(ErrorKind::Other);
        let err = state
            .child("kid")
            .map(|_| ())
            .expect_err("directory sync failed");
        assert!(matches!(err, FsError::DirSyncFailed { .. }), "{err:?}");
        drop(guard);
        // The directory exists now but its link was never synced. A retry finds an existing
        // directory and syncs nothing (the residual in the `atomic` module docs, section 3), and
        // so does every later call.
        let before = dir_sync_count();
        state.child("kid").expect("the retry finds the directory");
        state.child("kid").expect("and so does a later call");
        assert_eq!(
            dir_sync_count(),
            before,
            "an existing child is returned without a sync"
        );
    }

    #[test]
    fn creating_a_new_child_makes_exactly_one_directory_sync() {
        let (_g, dir) = tmp();
        let state = StateDir::create(&dir.join("s")).unwrap();
        let before = dir_sync_count();
        state.child("fresh").unwrap();
        assert_eq!(dir_sync_count() - before, 1);
    }

    #[test]
    fn a_successful_write_syncs_the_directory_once() {
        let (_g, dir) = tmp();
        let state = StateDir::create(&dir.join("s")).unwrap();
        let before = dir_sync_count();
        state.write("a", b"x").unwrap();
        assert_eq!(dir_sync_count() - before, 1);
    }
}

// ------------------------------------------------------------ 3: names

#[cfg(unix)]
#[test]
fn an_existing_name_over_the_write_cap_still_lists_reads_and_removes() {
    let (_g, dir) = tmp();
    let state = StateDir::create(&dir.join("s")).unwrap();
    let long = "n".repeat(230);
    std::fs::write(dir.join("s").join(&long), b"on-disk").unwrap();
    state.write("short", b"x").unwrap();
    let mut names = state.list(10).unwrap();
    names.sort();
    assert_eq!(names, vec![long.clone(), "short".to_string()]);
    assert_eq!(state.read(&long, 100).unwrap(), b"on-disk");
    assert!(state.exists(&long).unwrap());
    assert!(state.remove(&long).unwrap());
    // The write cap still holds by default.
    assert!(matches!(
        state.write(&"n".repeat(230), b"x"),
        Err(FsError::BadName(_))
    ));
}

#[cfg(unix)]
#[test]
fn the_write_cap_is_configurable_up_to_the_platform_limit() {
    use rightkit_fs::StateOptions;
    let (_g, dir) = tmp();
    let wide = StateOptions {
        max_name_len: Some(255),
        ..StateOptions::default()
    };
    let state = StateDir::create_with(&dir.join("s"), &wide).unwrap();
    let name = "w".repeat(255);
    state.write(&name, b"wide").unwrap();
    state.write_new(&"x".repeat(250), b"x").unwrap();
    assert_eq!(state.read(&name, 10).unwrap(), b"wide");
    assert_eq!(state.list(10).unwrap().len(), 2, "no temp leftovers");
    assert!(matches!(
        state.write(&"w".repeat(256), b"x"),
        Err(FsError::BadName(_))
    ));
    // A cap above the platform limit is clamped to it.
    let silly = StateOptions {
        max_name_len: Some(10_000),
        ..StateOptions::default()
    };
    let state = StateDir::open_with(&dir.join("s"), &silly).unwrap();
    assert!(matches!(
        state.write(&"w".repeat(256), b"x"),
        Err(FsError::BadName(_))
    ));
    // A narrow cap applies to writes only.
    let narrow = StateOptions {
        max_name_len: Some(5),
        ..StateOptions::default()
    };
    let state = StateDir::open_with(&dir.join("s"), &narrow).unwrap();
    assert!(matches!(
        state.write("sixsix", b"x"),
        Err(FsError::BadName(_))
    ));
    assert_eq!(state.read(&name, 10).unwrap(), b"wide");
}

// ------------------------------------------------------------ 4: sync level and policy

#[test]
fn durable_sync_with_never_reports_a_weaker_level_than_it_reached() {
    use rightkit_fs::{durable_sync_with, SyncLevel, SyncPolicy};
    let (_g, dir) = tmp();
    let file = std::fs::File::create(dir.join("f")).unwrap();
    // Strict is the strongest flush or an error, never a weaker success.
    if let Ok(level) = durable_sync_with(&file, SyncPolicy::Strict) {
        assert_eq!(level, SyncLevel::Full);
    }
    durable_sync_with(&file, SyncPolicy::Fallback).unwrap();
}

#[cfg(feature = "testing")]
mod policy {
    use super::*;
    use rightkit_fs::state::testing::without_full_sync;
    use rightkit_fs::{StateOptions, SyncLevel, SyncPolicy};

    #[test]
    fn fallback_reports_the_weaker_level_it_settled_for() {
        let (_g, dir) = tmp();
        let state = StateDir::create(&dir.join("s")).unwrap();
        let _no_full = without_full_sync();
        let level = state.write_reporting("a", b"x").unwrap();
        assert!(level < SyncLevel::Full, "{level:?}");
        assert!(state.write_new_reporting("b", b"x").unwrap() < SyncLevel::Full);
        assert_eq!(state.read("a", 10).unwrap(), b"x");
    }

    #[test]
    fn strict_refuses_to_publish_when_the_full_flush_is_unavailable() {
        let (_g, dir) = tmp();
        let strict = StateOptions {
            sync: SyncPolicy::Strict,
            ..StateOptions::default()
        };
        let state = StateDir::create_with(&dir.join("s"), &strict).unwrap();
        {
            let _no_full = without_full_sync();
            assert!(state.write("a", b"x").is_err());
            assert!(state.write_new("b", b"x").is_err());
        }
        assert!(
            state.list(10).unwrap().is_empty(),
            "nothing published, no temp left"
        );
        state
            .write("a", b"x")
            .expect("strict works when the flush is available");
    }
}

// ------------------------------------------------------------ 5: listing with kinds

#[test]
fn list_entries_returns_kinds_without_a_stat_per_entry() {
    use rightkit_fs::{DirEntry, EntryKind};
    let (_g, dir) = tmp();
    let state = StateDir::create(&dir.join("s")).unwrap();
    state.write("file", b"x").unwrap();
    state.child("sub").unwrap();
    for i in 0..40 {
        state.write(&format!("bulk-{i}"), b"x").unwrap();
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("file", dir.join("s/link")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(dir.join("s/sock")).unwrap();
    }
    #[cfg(feature = "testing")]
    let stats_before = rightkit_fs::state::testing::fallback_stat_count();
    let entries = state.list_entries(100).unwrap();
    #[cfg(feature = "testing")]
    assert_eq!(
        rightkit_fs::state::testing::fallback_stat_count(),
        stats_before,
        "this filesystem reports d_type, so no entry needs a stat"
    );
    let kind = |name: &str| {
        entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} missing from {entries:?}"))
            .kind
    };
    assert_eq!(kind("file"), EntryKind::File);
    assert_eq!(kind("sub"), EntryKind::Dir);
    assert_eq!(kind("bulk-7"), EntryKind::File);
    #[cfg(unix)]
    {
        assert_eq!(kind("link"), EntryKind::Symlink);
        assert_eq!(kind("sock"), EntryKind::Other);
    }
    assert!(entries.iter().all(|e| e.name != "." && e.name != ".."));
    // `list` is the names of the same entries.
    let mut names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    let mut listed = state.list(100).unwrap();
    names.sort();
    listed.sort();
    assert_eq!(names, listed);
    assert!(matches!(
        state.list_entries(3),
        Err(FsError::TooManyEntries(3))
    ));
    let _: Option<DirEntry> = entries.into_iter().next();
}

// ------------------------------------------------------------ 2 and 7: options

#[test]
fn a_directory_renamed_while_open_keeps_receiving_writes() {
    use rightkit_fs::StateOptions;
    let (_g, dir) = tmp();
    let options = StateOptions {
        allow_rename_while_open: true,
        ..StateOptions::default()
    };
    let path = dir.join("cleanup");
    let state = StateDir::create_with(&path, &options).unwrap();
    state.write("before", b"1").unwrap();
    let moved = dir.join("cleanup-canonical");
    std::fs::rename(&path, &moved).expect("rename while open is allowed by the option");
    std::fs::create_dir(&path).unwrap();
    // Handle-relative operations follow the original directory, not the path.
    state.write("after", b"2").unwrap();
    assert_eq!(state.read("before", 10).unwrap(), b"1");
    assert_eq!(std::fs::read(moved.join("after")).unwrap(), b"2");
    assert!(std::fs::read_dir(&path).unwrap().next().is_none());
    assert!(state.remove("before").unwrap());
    assert!(!moved.join("before").exists());
}

#[test]
fn the_privacy_option_leaves_unix_directories_owner_only() {
    use rightkit_fs::{Privacy, StateOptions};
    let (_g, dir) = tmp();
    for privacy in [
        Privacy::OwnerOnly,
        Privacy::OwnerOnlyUnixInheritWindows,
        Privacy::Inherit,
    ] {
        let options = StateOptions {
            privacy,
            ..StateOptions::default()
        };
        let path = dir.join(format!("{privacy:?}/deep"));
        let state = StateDir::create_with(&path, &options).unwrap();
        state.write("f", b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(path.clone()), 0o700, "{privacy:?}");
            assert_eq!(mode(path.join("f")), 0o600, "{privacy:?}");
        }
    }
}

// ------------------------------------------------------------ 6: read limits

#[test]
fn read_with_no_limit_returns_the_content_instead_of_overflowing() {
    let (_g, dir) = tmp();
    let state = StateDir::create(&dir.join("s")).unwrap();
    state.write("record", b"unlimited").unwrap();
    // `u64::MAX` is "no limit": the one-byte-over probe must saturate, not wrap to a zero-byte
    // read (release) or panic (debug).
    assert_eq!(state.read("record", u64::MAX).unwrap(), b"unlimited");
    assert_eq!(
        state.read_opt("record", u64::MAX).unwrap().unwrap(),
        b"unlimited"
    );
    state.write("empty", b"").unwrap();
    assert_eq!(state.read("empty", u64::MAX).unwrap(), b"");
}

#[test]
fn read_limits_are_exact_at_the_boundary() {
    let (_g, dir) = tmp();
    let state = StateDir::create(&dir.join("s")).unwrap();
    state.write("record", b"12345").unwrap();
    assert_eq!(
        state.read("record", 5).unwrap(),
        b"12345",
        "exactly at the limit"
    );
    assert_eq!(state.read("record", u64::MAX - 1).unwrap(), b"12345");
    for limit in [0, 4] {
        assert!(
            matches!(
                state.read("record", limit),
                Err(FsError::TooLarge { size: 5, limit: l, .. }) if l == limit
            ),
            "limit {limit}"
        );
    }
}

// ------------------------------------------------------------ 7: concurrent listings

#[test]
fn concurrent_listings_each_see_every_entry() {
    // Enough names that one `getdents` buffer cannot hold them all, so a listing needs several
    // reads of its directory stream. Listings that shared a file offset (duplicated descriptors)
    // would consume each other's position and come back short.
    const ENTRIES: usize = 4000;
    let (_g, dir) = tmp();
    let state = StateDir::create(&dir.join("s")).unwrap();
    for i in 0..ENTRIES {
        std::fs::write(
            dir.join("s")
                .join(format!("entry-{i:06}-padding-to-lengthen-names")),
            b"",
        )
        .unwrap();
    }
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|worker| {
                let state = &state;
                scope.spawn(move || {
                    for round in 0..25 {
                        let names = state.list(usize::MAX).unwrap();
                        assert_eq!(
                            names.len(),
                            ENTRIES,
                            "worker {worker} round {round}: a concurrent listing came back partial"
                        );
                        let entries = state.list_entries(usize::MAX).unwrap();
                        assert_eq!(entries.len(), ENTRIES, "worker {worker} round {round}");
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    });
}
