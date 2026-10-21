#![cfg(all(unix, feature = "lock"))]
//! Narrowing or inspecting an existing file must not release locks this process holds on it.
//!
//! A POSIX record lock belongs to the process, and closing ANY descriptor the process has on the
//! file releases it. `restrict_owner_only` and `private::is_owner_only` used to open and close
//! the file on macOS to read its ACL. rightkit-db applies `restrict_owner_only` to a database
//! and its `-wal` and `-shm` files on every open, so SQLite's own locks were dropped and a
//! second process could rebuild the WAL index under a live mapping (a consumer's engine died
//! with a bus error, 2026-10-12). The lock is observed from a second process: the test binary
//! runs itself as a child that tries the lock.

use std::path::Path;
use std::process::Command;

use rightkit_fs::{FileLock, FsError};

const PROBE: &str = "RIGHTKIT_FS_LOCK_PROBE";
/// Child exit codes.
const REFUSED: i32 = 0;
const ACQUIRED: i32 = 3;
const OTHER_ERROR: i32 = 4;

/// Runs only as the child: tries the lock named by the environment and reports by exit code.
#[test]
fn child_probe() {
    let Some(path) = std::env::var_os(PROBE) else {
        return;
    };
    let code = match FileLock::try_acquire(Path::new(&path)) {
        Ok(_) => ACQUIRED,
        Err(FsError::Locked { .. }) => REFUSED,
        Err(_) => OTHER_ERROR,
    };
    std::process::exit(code);
}

fn child_outcome(path: &Path) -> i32 {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_probe", "--nocapture"])
        .env(PROBE, path)
        .status()
        .unwrap()
        .code()
        .expect("the child exited by a signal")
}

#[test]
fn narrowing_and_inspecting_a_locked_file_keep_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("held.lock");
    let held = FileLock::try_acquire(&path).unwrap();
    assert_eq!(
        child_outcome(&path),
        REFUSED,
        "the lock is held to begin with"
    );

    rightkit_fs::restrict_owner_only(&path).unwrap();
    assert_eq!(
        child_outcome(&path),
        REFUSED,
        "restrict_owner_only released the lock this process holds on the file"
    );

    assert!(rightkit_fs::private::is_owner_only(&path).unwrap());
    assert_eq!(
        child_outcome(&path),
        REFUSED,
        "is_owner_only released the lock this process holds on the file"
    );

    drop(held);
    assert_eq!(child_outcome(&path), ACQUIRED, "free once released");
}
