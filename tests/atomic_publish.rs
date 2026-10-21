//! The order and the failure semantics of an atomic write, observed through the `testing` seam:
//! which flush, replace and directory-sync calls run and in what order (the power-loss argument),
//! that the owner-only restriction is in place before the replace publishes the file, that a
//! required hardening step failing leaves nothing published, and that a failure after publication
//! is typed, reported only after the required directory sync, and says the content is in place.

use std::cell::RefCell;
use std::io::Write;
use std::path::Path;
use std::rc::Rc;

use rightkit_fs::atomic::testing::{on_replace, take_steps, Step};
use rightkit_fs::{atomic_write_with_options, AtomicWriteOptions};
#[cfg(unix)]
use rightkit_fs::{FsError, ReplaceMode};

/// What a hook saw just before the rename: the temp file's mode and the destination's content.
#[cfg(unix)]
type Observed = (u32, Option<Vec<u8>>);

fn write(path: &Path, options: AtomicWriteOptions, bytes: &[u8]) -> rightkit_fs::Result<()> {
    atomic_write_with_options(path, options, |f| f.write_all(bytes))
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// ------------------------------------------------------------------ order of the sync calls

#[cfg(unix)]
#[test]
fn a_durable_write_flushes_the_temp_then_replaces_then_syncs_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("state.json");
    let _ = take_steps();
    write(&target, AtomicWriteOptions::default(), b"x").unwrap();
    assert_eq!(
        take_steps(),
        vec![
            Step::TempFlushed,
            Step::Replace,
            Step::DirSynced(dir.path().to_path_buf())
        ],
        "the file is flushed before the rename and the directory after it"
    );
}

#[cfg(windows)]
#[test]
fn a_durable_write_on_windows_flushes_the_temp_and_claims_no_directory_sync() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("state.json");
    let _ = take_steps();
    write(&target, AtomicWriteOptions::default(), b"x").unwrap();
    assert_eq!(take_steps(), vec![Step::TempFlushed, Step::Replace]);
}

#[test]
fn a_non_durable_write_flushes_and_syncs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("cache.bin");
    let options = AtomicWriteOptions {
        durable: false,
        ..AtomicWriteOptions::default()
    };
    let _ = take_steps();
    write(&target, options, b"x").unwrap();
    assert_eq!(take_steps(), vec![Step::Replace]);
}

#[test]
fn a_failed_replace_syncs_no_directory_and_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("state.json");
    std::fs::write(&target, b"old").unwrap();
    let _ = take_steps();
    let result = {
        let _guard = on_replace(|_| Some(std::io::Error::other("injected replace failure")));
        write(&target, AtomicWriteOptions::default(), b"new")
    };
    let error = result.expect_err("the replace failed");
    assert!(!error.target_replaced(), "{error:?}");
    assert_eq!(take_steps(), vec![Step::TempFlushed, Step::Replace]);
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert_eq!(names(dir.path()), vec!["state.json".to_string()]);
}

#[cfg(unix)]
#[test]
fn no_replace_flushes_publishes_exclusively_then_syncs_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("once.json");
    let options = AtomicWriteOptions {
        replace: ReplaceMode::NoReplace,
        ..AtomicWriteOptions::default()
    };
    let _ = take_steps();
    write(&target, options, b"x").unwrap();
    assert_eq!(
        take_steps(),
        vec![
            Step::TempFlushed,
            Step::Replace,
            Step::DirSynced(dir.path().to_path_buf())
        ]
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"x");
}

#[cfg(unix)]
#[test]
fn a_no_replace_loser_syncs_no_directory_and_leaves_the_winner() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("once.json");
    std::fs::write(&target, b"winner").unwrap();
    let options = AtomicWriteOptions {
        replace: ReplaceMode::NoReplace,
        ..AtomicWriteOptions::default()
    };
    let _ = take_steps();
    let error = write(&target, options, b"loser").expect_err("the name is taken");
    assert!(matches!(error, FsError::AlreadyExists(_)), "{error:?}");
    assert!(!error.target_replaced());
    assert_eq!(take_steps(), vec![Step::TempFlushed, Step::Replace]);
    assert_eq!(std::fs::read(&target).unwrap(), b"winner");
    assert_eq!(names(dir.path()), vec!["once.json".to_string()]);
}

// ------------------------------------------------ owner-only before publication (Unix, by mode)

#[cfg(unix)]
#[test]
fn the_temp_is_owner_only_before_the_replace_publishes_it() {
    use rightkit_fs::Privacy;
    use std::os::unix::fs::PermissionsExt;

    let base = AtomicWriteOptions::default();
    let cases = [
        (
            "private",
            AtomicWriteOptions {
                private: true,
                ..base
            },
        ),
        (
            "OwnerOnly",
            AtomicWriteOptions {
                privacy: Privacy::OwnerOnly,
                ..base
            },
        ),
        (
            "OwnerOnlyUnixInheritWindows",
            AtomicWriteOptions {
                privacy: Privacy::OwnerOnlyUnixInheritWindows,
                ..base
            },
        ),
    ];
    for (label, options) in cases {
        for existing in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("secret.json");
            if existing {
                std::fs::write(&target, b"old").unwrap();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            // At the instant before the rename: the temp's mode, and what the destination name
            // shows (the old content, or nothing). Nothing readable by others may ever be
            // published under the destination name.
            let seen: Rc<RefCell<Vec<Observed>>> = Rc::default();
            let sink = seen.clone();
            let guard = on_replace(move |call| {
                let mode = std::fs::metadata(&call.temp).unwrap().permissions().mode() & 0o777;
                sink.borrow_mut()
                    .push((mode, std::fs::read(&call.target).ok()));
                None
            });
            write(&target, options, b"secret").unwrap();
            drop(guard);

            let seen = seen.borrow();
            assert_eq!(seen.len(), 1, "{label}: one replace call");
            assert_eq!(
                seen[0].0 & 0o077,
                0,
                "{label} existing={existing}: the temp was group/other accessible when published ({:o})",
                seen[0].0
            );
            assert_eq!(
                seen[0].1,
                existing.then(|| b"old".to_vec()),
                "{label}: the new content must not be visible before the replace"
            );
            let published = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                published & 0o077,
                0,
                "{label}: published mode {published:o}"
            );
            assert_eq!(std::fs::read(&target).unwrap(), b"secret");
        }
    }
}

// ----------------------------------------- post-publication failures (Unix link fallback)

#[cfg(unix)]
mod post_publication {
    use super::*;
    use rightkit_fs::atomic::testing::{fail_dir_sync, force_link_fallback};
    use rightkit_fs::{rename_exclusive, PostPublishStep, ReplaceMode};
    use std::io::ErrorKind;

    fn no_replace() -> AtomicWriteOptions {
        AtomicWriteOptions {
            replace: ReplaceMode::NoReplace,
            ..AtomicWriteOptions::default()
        }
    }

    #[test]
    fn a_failed_unlink_after_the_link_is_a_typed_error_and_the_directory_is_still_synced() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("once.bin");
        let _ = take_steps();
        let result = {
            let _fallback = force_link_fallback(true);
            write(&target, no_replace(), b"published")
        };
        let error = result.expect_err("the cleanup step failed");
        assert!(
            matches!(
                &error,
                FsError::ReplacedButPostStepFailed { path, step: PostPublishStep::TempCleanup, .. }
                    if path == &target
            ),
            "{error:?}"
        );
        assert!(
            error.target_replaced(),
            "published content is reported as such"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"published");
        assert_eq!(
            take_steps(),
            vec![
                Step::TempFlushed,
                Step::Replace,
                Step::DirSynced(dir.path().to_path_buf())
            ],
            "the required directory sync ran before the error was returned"
        );
        assert_eq!(
            names(dir.path()),
            vec!["once.bin".to_string()],
            "the leftover second name is removed on the way out"
        );
    }

    #[test]
    fn the_link_fallback_publishes_cleanly_when_the_unlink_works() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("once.bin");
        {
            let _fallback = force_link_fallback(false);
            write(&target, no_replace(), b"first").unwrap();
            let again = write(&target, no_replace(), b"second").unwrap_err();
            assert!(matches!(again, FsError::AlreadyExists(_)), "{again:?}");
            assert!(!again.target_replaced());
        }
        assert_eq!(std::fs::read(&target).unwrap(), b"first");
        assert_eq!(names(dir.path()), vec!["once.bin".to_string()]);
    }

    #[test]
    fn a_failed_directory_sync_outranks_a_failed_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("once.bin");
        let result = {
            let _sync = fail_dir_sync(ErrorKind::Other);
            let _fallback = force_link_fallback(true);
            write(&target, no_replace(), b"published")
        };
        let error = result.expect_err("both post-publication steps failed");
        assert!(
            matches!(&error, FsError::ReplacedButDirSyncFailed { .. }),
            "{error:?}"
        );
        assert!(error.target_replaced());
        assert_eq!(std::fs::read(&target).unwrap(), b"published");
    }

    #[test]
    fn rename_exclusive_reports_a_published_target_whose_source_name_could_not_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("from"), dir.path().join("to"));
        std::fs::write(&from, b"payload").unwrap();
        let error = {
            let _fallback = force_link_fallback(true);
            rename_exclusive(&from, &to).expect_err("the unlink failed")
        };
        assert!(
            matches!(
                &error,
                FsError::ReplacedButPostStepFailed { path, step: PostPublishStep::TempCleanup, .. }
                    if path == &to
            ),
            "{error:?}"
        );
        assert!(error.target_replaced());
        assert_eq!(std::fs::read(&to).unwrap(), b"payload", "the target exists");
        assert!(from.exists(), "the source name is still there, as reported");
    }
}

// ------------------------------------------------- parent directories are published (Unix)

#[cfg(unix)]
mod parents {
    use super::*;
    use rightkit_fs::atomic::testing::fail_dir_sync;

    fn with_parents() -> AtomicWriteOptions {
        AtomicWriteOptions {
            create_parents: true,
            ..AtomicWriteOptions::default()
        }
    }

    // The temp root exists before each test starts, so its ancestors are never created and the
    // step logs below are exact: they hold only what the write under test did.

    #[test]
    fn every_created_ancestor_is_published_before_the_file_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let target = root.join("new/deep/settings.json");
        let _ = take_steps();
        write(&target, with_parents(), b"x").unwrap();
        assert_eq!(
            take_steps(),
            vec![
                Step::DirSynced(root.clone()),
                Step::DirSynced(root.join("new")),
                Step::TempFlushed,
                Step::Replace,
                Step::DirSynced(root.join("new/deep")),
            ],
            "each new directory's link is flushed, top down, before the file below it"
        );
    }

    /// The residual, asserted: the first attempt created `first`, and the sync of its link failed.
    /// The retry finds `first` as an existing directory and does not sync it; it creates and
    /// publishes `second` and completes the write.
    #[test]
    fn a_failed_ancestor_sync_stops_the_write_and_the_retry_syncs_only_what_it_creates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let target = root.join("first/second/settings.json");
        let _ = take_steps();
        let guard = fail_dir_sync(std::io::ErrorKind::Other);
        let error = write(&target, with_parents(), b"x").expect_err("the sync failed");
        drop(guard);
        assert!(
            matches!(&error, FsError::DirSyncFailed { path, .. } if path == &root.join("first")),
            "{error:?}"
        );
        assert!(!target.exists());
        assert!(root.join("first").is_dir(), "the directory stays");
        let _ = take_steps();
        write(&target, with_parents(), b"x").unwrap();
        assert_eq!(
            take_steps(),
            vec![
                Step::DirSynced(root.join("first")),
                Step::TempFlushed,
                Step::Replace,
                Step::DirSynced(root.join("first/second")),
            ],
            "`first` is found and left alone; only `second`, which this call created, is synced"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"x");
    }

    /// A real `mkdir` in another process stands for a crashed earlier run or a racing thread. The
    /// directory exists, so the write finds it and does not sync it; only the directory the write
    /// itself creates below it is published.
    #[test]
    fn a_pre_existing_directory_is_not_synced_only_what_the_call_creates_is() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let status = std::process::Command::new("mkdir")
            .arg(root.join("found"))
            .status()
            .unwrap();
        assert!(status.success());
        let _ = take_steps();
        write(
            &root.join("found/inner/settings.json"),
            with_parents(),
            b"x",
        )
        .unwrap();
        assert_eq!(
            take_steps(),
            vec![
                Step::DirSynced(root.join("found")),
                Step::TempFlushed,
                Step::Replace,
                Step::DirSynced(root.join("found/inner")),
            ],
            "`found` is left alone; `inner` is created and published through its parent"
        );
    }

    /// Steady state: nothing remembers an earlier sync and nothing walks existing directories, so
    /// a second identical write makes no ancestor sync. What remains is the write's own flush,
    /// replace and sync of the destination directory.
    #[test]
    fn a_second_identical_write_syncs_no_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write(&root.join("a/b/one.json"), with_parents(), b"1").unwrap();
        let _ = take_steps();
        write(&root.join("a/b/two.json"), with_parents(), b"2").unwrap();
        assert_eq!(
            take_steps(),
            vec![
                Step::TempFlushed,
                Step::Replace,
                Step::DirSynced(root.join("a/b"))
            ],
            "only the write's own directory sync remains"
        );
    }

    /// A directory moved into an existing one is, to the next write, an existing directory like
    /// any other: found, not synced (the residual in the module docs).
    #[test]
    fn a_directory_moved_into_place_is_a_pre_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write(&root.join("a/dir/one.json"), with_parents(), b"1").unwrap();
        std::fs::create_dir(root.join("b")).unwrap();
        std::fs::rename(root.join("a/dir"), root.join("b/dir")).unwrap();
        let _ = take_steps();
        write(&root.join("b/dir/two.json"), with_parents(), b"2").unwrap();
        assert_eq!(
            take_steps(),
            vec![
                Step::TempFlushed,
                Step::Replace,
                Step::DirSynced(root.join("b/dir"))
            ],
            "nothing is created, so no ancestor is synced"
        );
    }

    #[test]
    fn a_non_durable_write_makes_parents_without_syncing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a/b/cache.bin");
        let options = AtomicWriteOptions {
            durable: false,
            ..with_parents()
        };
        let _ = take_steps();
        write(&target, options, b"x").unwrap();
        assert_eq!(take_steps(), vec![Step::Replace]);
    }
}

// ------------------------------------ Windows: creation-time descriptor and fail-closed hardening

#[cfg(windows)]
mod windows {
    use super::*;
    use rightkit_fs::atomic::testing::on_harden;
    use rightkit_fs::atomic::HardenStage;
    use rightkit_fs::{Privacy, ReplaceMode};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetNamedSecurityInfoW(
            path: *const u16,
            object_type: u32,
            information: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor: *mut c_void,
            revision: u32,
            information: u32,
            out: *mut *mut u16,
            len: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    /// The DACL of `path` as SDDL, with the `AI` bookkeeping flag normalised away.
    fn dacl_sddl(path: &Path) -> String {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let mut descriptor = std::ptr::null_mut();
            let status = GetNamedSecurityInfoW(
                wide.as_ptr(),
                1,
                4,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            );
            assert_eq!(status, 0, "GetNamedSecurityInfoW({})", path.display());
            let mut text = std::ptr::null_mut();
            assert_ne!(
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    1,
                    4,
                    &mut text,
                    std::ptr::null_mut()
                ),
                0
            );
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let sddl = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            LocalFree(descriptor);
            sddl.replacen("D:PAI", "D:P", 1)
        }
    }

    fn private(mode: ReplaceMode) -> AtomicWriteOptions {
        AtomicWriteOptions {
            private: true,
            replace: mode,
            ..AtomicWriteOptions::default()
        }
    }

    /// Owner-only: protected, exactly one ACE, none for a broad principal.
    fn assert_owner_only(sddl: &str, what: &str) {
        assert!(sddl.starts_with("D:P("), "{what} must be protected: {sddl}");
        assert_eq!(sddl.matches('(').count(), 1, "{what}: one ACE: {sddl}");
        for broad in [";;;WD)", ";;;AU)", ";;;BU)", ";;;IU)"] {
            assert!(
                !sddl.contains(broad),
                "{what} names a broad principal: {sddl}"
            );
        }
    }

    #[test]
    fn the_temp_carries_its_owner_only_descriptor_from_creation_and_is_never_restricted_later() {
        for mode in [ReplaceMode::Rename, ReplaceMode::MoveWriteThrough] {
            for existing in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let target = dir.path().join("secret.json");
                if existing {
                    std::fs::write(&target, b"old").unwrap();
                }
                let log: Rc<RefCell<Vec<String>>> = Rc::default();
                let (harden_log, replace_log) = (log.clone(), log.clone());
                let _harden = on_harden(move |call| {
                    harden_log.borrow_mut().push(format!("{:?}", call.stage));
                    None
                });
                let _replace = on_replace(move |call| {
                    // The descriptor the temp has at the instant it is published.
                    replace_log
                        .borrow_mut()
                        .push(format!("Replace {}", dacl_sddl(&call.temp)));
                    None
                });
                write(&target, private(mode), b"secret").unwrap();

                let log = log.borrow();
                let at_publication = log
                    .iter()
                    .find_map(|entry| entry.strip_prefix("Replace "))
                    .expect("the replace ran");
                assert_owner_only(at_publication, "the temp at publication");
                assert_eq!(
                    log[0], "TempCreate",
                    "{mode:?} existing={existing}: the descriptor is applied at creation"
                );
                assert_eq!(
                    log.iter().filter(|e| *e == "TempCreate").count(),
                    1,
                    "{mode:?}: and the temp is never restricted again"
                );
                // No step touches the destination: the temp's descriptor travels with it.
                assert_eq!(log.len(), 2, "{mode:?}: creation, then the replace only");
                assert_owner_only(&dacl_sddl(&target), "the published file");
                assert_eq!(std::fs::read(&target).unwrap(), b"secret");
            }
        }
    }

    #[test]
    fn a_failed_owner_only_creation_publishes_nothing_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret.json");
        std::fs::write(&target, b"old").unwrap();
        let _ = take_steps();
        let result = {
            let _harden = on_harden(|call| {
                (call.stage == HardenStage::TempCreate)
                    .then(|| std::io::Error::other("injected ACL failure"))
            });
            write(&target, private(ReplaceMode::Rename), b"secret")
        };
        let error = result.expect_err("a private write must fail closed");
        assert!(!error.target_replaced(), "{error:?}");
        assert!(
            error.to_string().contains("injected ACL failure"),
            "{error}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(names(dir.path()), vec!["secret.json".to_string()]);
        assert!(
            !take_steps().contains(&Step::Replace),
            "nothing reached the replace call"
        );
    }

    #[test]
    fn inherit_privacy_takes_no_owner_only_step() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("plain.json");
        let stages: Rc<RefCell<Vec<HardenStage>>> = Rc::default();
        let sink = stages.clone();
        let _harden = on_harden(move |call| {
            sink.borrow_mut().push(call.stage);
            None
        });
        let options = AtomicWriteOptions {
            privacy: Privacy::OwnerOnlyUnixInheritWindows,
            ..AtomicWriteOptions::default()
        };
        write(&target, options, b"x").unwrap();
        assert!(stages.borrow().is_empty());
    }

    fn with_parents() -> AtomicWriteOptions {
        AtomicWriteOptions {
            create_parents: true,
            ..AtomicWriteOptions::default()
        }
    }

    // The temp root exists before each test starts, so its own ancestors are never created and
    // the step logs below hold only what the write under test did. Each created directory is
    // flushed, then its parent (which holds its link), before the next component.

    #[test]
    fn created_parents_are_flushed_top_down_before_the_file_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let target = root.join("new").join("deep").join("settings.json");
        let _ = take_steps();
        write(&target, with_parents(), b"x").unwrap();
        assert_eq!(
            take_steps(),
            vec![
                Step::DirSynced(root.join("new")),
                Step::DirSynced(root.clone()),
                Step::DirSynced(root.join("new").join("deep")),
                Step::DirSynced(root.join("new")),
                Step::TempFlushed,
                Step::Replace,
            ],
            "each created directory and its parent are flushed before the file below them"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"x");
    }

    #[test]
    fn a_pre_existing_directory_is_not_flushed_only_what_the_call_creates_is() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join("found")).unwrap();
        let target = root.join("found").join("inner").join("settings.json");
        let _ = take_steps();
        write(&target, with_parents(), b"x").unwrap();
        assert_eq!(
            take_steps(),
            vec![
                Step::DirSynced(root.join("found").join("inner")),
                Step::DirSynced(root.join("found")),
                Step::TempFlushed,
                Step::Replace,
            ],
            "`found` is only the parent of the one directory this call created"
        );
        // A second identical call finds every directory and flushes none.
        write(&target, with_parents(), b"y").unwrap();
        assert_eq!(take_steps(), vec![Step::TempFlushed, Step::Replace]);
    }

    #[test]
    fn a_non_durable_write_makes_parents_without_flushing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a").join("b").join("cache.bin");
        let options = AtomicWriteOptions {
            durable: false,
            create_parents: true,
            ..AtomicWriteOptions::default()
        };
        let _ = take_steps();
        write(&target, options, b"x").unwrap();
        assert_eq!(take_steps(), vec![Step::Replace]);
    }

    #[test]
    fn private_parents_are_flushed_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let target = root.join("secret").join("state.json");
        let options = AtomicWriteOptions {
            create_parents: true,
            privacy: Privacy::OwnerOnly,
            ..AtomicWriteOptions::default()
        };
        let _ = take_steps();
        write(&target, options, b"x").unwrap();
        let steps = take_steps();
        assert_eq!(
            &steps[..2],
            &[
                Step::DirSynced(root.join("secret")),
                Step::DirSynced(root.clone()),
            ],
            "{steps:?}"
        );
    }

    #[test]
    fn a_failed_parent_flush_is_a_dir_sync_failed_and_the_directory_stays() {
        use rightkit_fs::atomic::testing::fail_dir_sync;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let target = root.join("first").join("second").join("settings.json");
        let _ = take_steps();
        let error = {
            let _guard = fail_dir_sync(std::io::ErrorKind::Other);
            write(&target, with_parents(), b"x").expect_err("the flush failed")
        };
        assert!(
            matches!(&error, rightkit_fs::FsError::DirSyncFailed { path, .. }
                if path == &root.join("first")),
            "{error:?}"
        );
        assert!(!target.exists(), "nothing was written below it");
        assert!(root.join("first").is_dir(), "the directory stays");
        let _ = take_steps();
        write(&target, with_parents(), b"x").unwrap();
        assert_eq!(
            take_steps().first(),
            Some(&Step::DirSynced(root.join("first").join("second"))),
            "the retry finds `first` and flushes only what it creates"
        );
    }
}
