//! No-replace publishing: `ReplaceMode::NoReplace` and `rename_exclusive` fail with a typed error when
//! the destination exists, atomically, and a losing writer leaves no temp file.

use std::io::Write;
use std::sync::{Arc, Barrier};

use rightkit_fs::{
    atomic_write_with_options, rename_exclusive, AtomicWriteOptions, FsError, ReplaceMode,
};

fn no_replace() -> AtomicWriteOptions {
    AtomicWriteOptions {
        replace: ReplaceMode::NoReplace,
        ..AtomicWriteOptions::default()
    }
}

fn write(path: &std::path::Path, bytes: Vec<u8>) -> rightkit_fs::Result<()> {
    atomic_write_with_options(path, no_replace(), move |f| f.write_all(&bytes))
}

#[test]
fn publishes_when_absent_and_refuses_when_present_without_touching_the_old_file() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("once.bin");
    write(&target, b"first".to_vec()).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"first");

    let error = write(&target, b"second".to_vec()).unwrap_err();
    assert!(
        matches!(&error, FsError::AlreadyExists(p) if p == &target),
        "{error:?}"
    );
    assert_eq!(error.kind(), Some(std::io::ErrorKind::AlreadyExists));
    assert_eq!(std::fs::read(&target).unwrap(), b"first");
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
    assert_eq!(names.len(), 1, "the refused writer's temp file is removed");
}

#[test]
fn rename_exclusive_moves_when_absent_and_keeps_both_files_when_present() {
    let dir = tempfile::tempdir().unwrap();
    let (from, to) = (dir.path().join("from"), dir.path().join("to"));
    std::fs::write(&from, b"a").unwrap();
    rename_exclusive(&from, &to).unwrap();
    assert!(!from.exists());
    assert_eq!(std::fs::read(&to).unwrap(), b"a");

    std::fs::write(&from, b"b").unwrap();
    let error = rename_exclusive(&from, &to).unwrap_err();
    assert!(
        matches!(&error, FsError::AlreadyExists(p) if p == &to),
        "{error:?}"
    );
    assert_eq!(std::fs::read(&from).unwrap(), b"b", "the source stays");
    assert_eq!(
        std::fs::read(&to).unwrap(),
        b"a",
        "the destination is untouched"
    );

    let missing = rename_exclusive(&dir.path().join("nope"), &dir.path().join("elsewhere"));
    assert!(
        matches!(missing, Err(FsError::Io { .. })),
        "a missing source is a plain I/O error"
    );
}

#[test]
fn racing_publishers_have_exactly_one_winner_and_the_losers_leave_no_temp() {
    const WRITERS: usize = 8;
    for round in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let target = Arc::new(dir.path().join("race.bin"));
        let gate = Arc::new(Barrier::new(WRITERS));
        let handles: Vec<_> = (0..WRITERS)
            .map(|n| {
                let (target, gate) = (target.clone(), gate.clone());
                std::thread::spawn(move || {
                    gate.wait();
                    let payload = format!("writer-{n}").into_bytes();
                    (n, write(&target, payload))
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners: Vec<_> = results.iter().filter(|(_, r)| r.is_ok()).collect();
        assert_eq!(
            winners.len(),
            1,
            "round {round}: exactly one publisher wins"
        );
        for (n, result) in &results {
            if let Err(error) = result {
                assert!(
                    matches!(error, FsError::AlreadyExists(_)),
                    "round {round} writer {n}: {error:?}"
                );
            }
        }
        let winner = winners[0].0;
        assert_eq!(
            std::fs::read(&*target).unwrap(),
            format!("writer-{winner}").into_bytes()
        );
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(names.len(), 1, "round {round}: no temp files left");
    }
}
