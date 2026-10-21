//! Exclusive publish on a volume with neither an exclusive rename nor hard links (FAT, exFAT, some
//! network shares). The volume is simulated by `testing::force_no_hard_links`, which skips the
//! exclusive rename and makes `link(2)` fail with `EPERM`; no FAT volume is involved. The publish
//! then claims the name with an `O_EXCL` placeholder and renames the finished file over it.
#![cfg(unix)]

use std::io::Write;
use std::sync::{Arc, Barrier};

use rightkit_fs::atomic::testing::{force_link_fallback, force_no_hard_links};
use rightkit_fs::{
    atomic_write_with_options, rename_exclusive, rename_exclusive_reporting, AtomicWriteOptions,
    ExclusiveMethod, FsError, ReplaceMode,
};

fn no_replace() -> AtomicWriteOptions {
    AtomicWriteOptions {
        replace: ReplaceMode::NoReplace,
        ..AtomicWriteOptions::default()
    }
}

#[test]
fn a_volume_without_hard_links_publishes_through_the_placeholder() {
    let dir = tempfile::tempdir().unwrap();
    let (from, to) = (dir.path().join("from"), dir.path().join("to"));
    std::fs::write(&from, b"finished").unwrap();
    let _no_links = force_no_hard_links();
    let method = rename_exclusive_reporting(&from, &to).unwrap();
    assert_eq!(method, ExclusiveMethod::PlaceholderThenRename);
    assert_eq!(std::fs::read(&to).unwrap(), b"finished");
    assert!(
        !from.exists(),
        "the source name is gone: it was renamed, not copied"
    );
}

#[test]
fn an_existing_destination_is_refused_and_both_files_stay() {
    let dir = tempfile::tempdir().unwrap();
    let (from, to) = (dir.path().join("from"), dir.path().join("to"));
    std::fs::write(&from, b"new").unwrap();
    std::fs::write(&to, b"old").unwrap();
    let _no_links = force_no_hard_links();
    let error = rename_exclusive(&from, &to).unwrap_err();
    assert!(
        matches!(&error, FsError::AlreadyExists(p) if p == &to),
        "{error:?}"
    );
    assert_eq!(
        std::fs::read(&to).unwrap(),
        b"old",
        "the winner is untouched"
    );
    assert_eq!(std::fs::read(&from).unwrap(), b"new", "the source stays");
}

#[test]
fn a_failed_rename_removes_the_placeholder_and_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (from, to) = (dir.path().join("missing"), dir.path().join("to"));
    let _no_links = force_no_hard_links();
    let error = rename_exclusive(&from, &to).unwrap_err();
    assert!(matches!(error, FsError::Io { .. }), "{error:?}");
    assert!(
        !to.exists(),
        "the empty placeholder this call made is removed"
    );
}

#[test]
fn no_replace_atomic_writes_use_the_placeholder_and_leave_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("out.bin");
    let _no_links = force_no_hard_links();
    atomic_write_with_options(&target, no_replace(), |f| f.write_all(b"first")).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"first");

    let error =
        atomic_write_with_options(&target, no_replace(), |f| f.write_all(b"second")).unwrap_err();
    assert!(matches!(error, FsError::AlreadyExists(_)), "{error:?}");
    assert_eq!(std::fs::read(&target).unwrap(), b"first");
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
    assert_eq!(names.len(), 1, "the loser's temp is removed: {names:?}");
}

#[test]
fn racing_publishers_without_hard_links_have_exactly_one_winner() {
    const WRITERS: usize = 8;
    for round in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let target = Arc::new(dir.path().join("race.bin"));
        let gate = Arc::new(Barrier::new(WRITERS));
        let handles: Vec<_> = (0..WRITERS)
            .map(|n| {
                let (target, gate) = (target.clone(), gate.clone());
                std::thread::spawn(move || {
                    // The seam is per thread: each racer installs its own.
                    let _no_links = force_no_hard_links();
                    gate.wait();
                    let bytes = format!("writer-{n}").into_bytes();
                    let result = atomic_write_with_options(&target, no_replace(), move |f| {
                        f.write_all(&bytes)
                    });
                    (n, result)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners: Vec<usize> = results
            .iter()
            .filter(|(_, r)| r.is_ok())
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(winners.len(), 1, "round {round}: {results:?}");
        for (n, result) in &results {
            if *n != winners[0] {
                assert!(
                    matches!(result, Err(FsError::AlreadyExists(_))),
                    "round {round}: {result:?}"
                );
            }
        }
        assert_eq!(
            std::fs::read(&*target).unwrap(),
            format!("writer-{}", winners[0]).into_bytes(),
            "round {round}: the winner's whole content"
        );
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(
            names.len(),
            1,
            "round {round}: losers left no temp: {names:?}"
        );
    }
}

#[test]
fn the_link_fallback_still_reports_link_then_unlink() {
    let dir = tempfile::tempdir().unwrap();
    let (from, to) = (dir.path().join("from"), dir.path().join("to"));
    std::fs::write(&from, b"x").unwrap();
    let _link = force_link_fallback(false);
    assert_eq!(
        rename_exclusive_reporting(&from, &to).unwrap(),
        ExclusiveMethod::LinkThenUnlink
    );
    assert_eq!(std::fs::read(&to).unwrap(), b"x");
}
