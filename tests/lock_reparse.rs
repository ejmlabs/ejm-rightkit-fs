//! A link at the lock file's name is refused, not followed (ledger row 111, point 6a).

use rightkit_fs::{FileLock, FsError, LockKind, LockOptions};
use std::time::Duration;

fn tmp() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

#[cfg(unix)]
fn link(target: &std::path::Path, at: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(windows)]
fn link(target: &std::path::Path, at: &std::path::Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, at)
}

fn assert_refused(lock: &std::path::Path) {
    for kind in [LockKind::Fcntl, LockKind::Flock] {
        let err = FileLock::acquire_kind(lock, kind, &LockOptions::wait(Duration::ZERO))
            .expect_err("a linked lock file must be refused");
        assert!(matches!(err, FsError::Symlink(_)), "{kind:?}: {err:?}");
        let err = FileLock::acquire_shared_kind(lock, kind, &LockOptions::wait(Duration::ZERO))
            .expect_err("a linked lock file must be refused (shared)");
        assert!(matches!(err, FsError::Symlink(_)), "{kind:?}: {err:?}");
    }
}

#[test]
fn a_symlink_to_an_existing_file_is_not_followed() {
    let (_g, dir) = tmp();
    let victim = dir.join("victim");
    std::fs::write(&victim, b"keep").unwrap();
    let lock = dir.join("app.lock");
    if let Err(e) = link(&victim, &lock) {
        eprintln!("SKIP: cannot create a symlink here: {e}");
        return;
    }
    let err = FileLock::acquire(&lock, &LockOptions::wait(Duration::ZERO))
        .expect_err("a linked lock file must be refused");
    assert!(matches!(err, FsError::Symlink(_)), "{err:?}");
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    assert_refused(&lock);
}

#[test]
fn a_dangling_symlink_is_not_created_through() {
    let (_g, dir) = tmp();
    let victim = dir.join("not-yet");
    let lock = dir.join("app.lock");
    if let Err(e) = link(&victim, &lock) {
        eprintln!("SKIP: cannot create a symlink here: {e}");
        return;
    }
    assert_refused(&lock);
    assert!(!victim.exists(), "the lock must not create the link target");
}

#[test]
fn a_regular_lock_file_still_locks() {
    let (_g, dir) = tmp();
    let lock = dir.join("app.lock");
    let held = FileLock::acquire(&lock, &LockOptions::wait(Duration::ZERO)).unwrap();
    assert!(FileLock::acquire(&lock, &LockOptions::wait(Duration::ZERO)).is_err());
    drop(held);
    FileLock::acquire(&lock, &LockOptions::wait(Duration::ZERO)).unwrap();
}
