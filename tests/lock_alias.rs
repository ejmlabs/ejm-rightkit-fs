#![cfg(unix)]
//! One lock file reached through two names is one lock. POSIX record locks belong to the process
//! and the file, so a second descriptor for the same inode neither conflicts with the first nor
//! may be closed while the first is held. The registry therefore keys by `(dev, ino)`, not by a
//! spelling of the path.
//!
//! Another process observes the real lock state (a second descriptor in this process would be
//! told nothing by the kernel): the test binary re-executes itself as a probe.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use rightkit_fs::{FileLock, FsError, LockKind, LockOptions};

const PROBE_ENV: &str = "RIGHTKIT_FS_TEST_LOCK_ALIAS_PROBE";

/// Runs in the re-executed child: try to take the exclusive record lock on the path in the env
/// var and report whether it was free.
#[test]
fn probe_entry_point() {
    let Ok(path) = std::env::var(PROBE_ENV) else {
        return;
    };
    match FileLock::try_lock_with(Path::new(&path), LockKind::Fcntl, Duration::ZERO) {
        Ok(_lock) => println!("PROBE FREE"),
        Err(_) => println!("PROBE BUSY"),
    }
}

/// Whether another process can take the exclusive record lock on `path` right now.
fn other_process_finds_it_free(path: &Path) -> bool {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "probe_entry_point",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE_ENV, path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("PROBE FREE") || text.contains("PROBE BUSY"),
        "the probe did not report: {text}"
    );
    text.contains("PROBE FREE")
}

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

fn zero() -> LockOptions {
    LockOptions::wait(Duration::ZERO)
}

/// A hard link: a second name for the same inode.
fn hard_link_pair(dir: &Path) -> (PathBuf, PathBuf) {
    let first = dir.join("a.lock");
    let second = dir.join("b.lock");
    std::fs::File::create(&first).unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    (first, second)
}

/// A case alias, when this volume is case-insensitive (default APFS and NTFS are; Linux ext4 is
/// not, and the case is skipped there).
fn case_pair(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let first = dir.join("case.lock");
    std::fs::File::create(&first).unwrap();
    let alias = dir.join("CASE.LOCK");
    alias.exists().then_some((first, alias))
}

fn assert_alias_cannot_downgrade_or_release(first: &Path, second: &Path) {
    let held = FileLock::acquire(first, &zero()).unwrap();
    assert!(
        !other_process_finds_it_free(first),
        "precondition: the exclusive hold is real"
    );

    // Shared and exclusive requests through the alias are refused: the holder is exclusive.
    let shared = FileLock::acquire_shared(second, &zero());
    assert!(
        matches!(shared, Err(FsError::LockTimeout { .. })),
        "a shared lock through an alias must be refused, not downgrade the exclusive hold: {shared:?}"
    );
    let exclusive = FileLock::try_acquire(second);
    assert!(
        matches!(exclusive, Err(FsError::Locked { .. })),
        "{exclusive:?}"
    );

    // The refused attempts closed nothing the holder needs.
    assert!(
        !other_process_finds_it_free(first),
        "a refused alias acquisition must not release the holder's lock"
    );
    drop(held);
    assert!(
        other_process_finds_it_free(first),
        "released once the real holder drops"
    );
}

fn assert_shared_aliases_release_only_with_the_last_holder(first: &Path, second: &Path) {
    let a = FileLock::acquire_shared(first, &zero()).unwrap();
    let b = FileLock::acquire_shared(second, &zero()).expect("shared holders coexist");
    drop(b);
    assert!(
        !other_process_finds_it_free(first),
        "dropping the alias holder must not release the other holder's lock"
    );
    drop(a);
    assert!(other_process_finds_it_free(first), "free after the last");

    // And the other order.
    let a = FileLock::acquire_shared(first, &zero()).unwrap();
    let b = FileLock::acquire_shared(second, &zero()).unwrap();
    drop(a);
    assert!(
        !other_process_finds_it_free(second),
        "dropping the first holder must not release the alias holder's lock"
    );
    drop(b);
    assert!(other_process_finds_it_free(second));
}

#[test]
fn a_hard_link_cannot_downgrade_or_release_an_exclusive_lock() {
    let (_g, dir) = tmp();
    let (first, second) = hard_link_pair(&dir);
    assert_alias_cannot_downgrade_or_release(&first, &second);
}

#[test]
fn hard_link_shared_holders_keep_the_lock_until_the_last_drops() {
    let (_g, dir) = tmp();
    let (first, second) = hard_link_pair(&dir);
    assert_shared_aliases_release_only_with_the_last_holder(&first, &second);
}

#[test]
fn a_case_alias_cannot_downgrade_or_release_an_exclusive_lock() {
    let (_g, dir) = tmp();
    let Some((first, second)) = case_pair(&dir) else {
        eprintln!("SKIP: this volume is case-sensitive");
        return;
    };
    assert_alias_cannot_downgrade_or_release(&first, &second);
    assert_shared_aliases_release_only_with_the_last_holder(&first, &second);
}

#[test]
fn a_symlinked_directory_reaches_the_same_lock() {
    let (_g, dir) = tmp();
    let real = dir.join("real");
    std::fs::create_dir(&real).unwrap();
    let via = dir.join("via");
    if std::os::unix::fs::symlink(&real, &via).is_err() {
        eprintln!("SKIP: cannot create a symlink here");
        return;
    }
    let held = FileLock::acquire(&real.join("x.lock"), &zero()).unwrap();
    assert!(FileLock::try_acquire(&via.join("x.lock")).is_err());
    assert!(!other_process_finds_it_free(&real.join("x.lock")));
    drop(held);
}

/// Count of open descriptors in this process, through the descriptor directory both macOS and
/// Linux expose.
fn open_descriptors() -> usize {
    std::fs::read_dir("/dev/fd")
        .map(|entries| entries.count())
        .unwrap_or(0)
}

#[test]
fn waiting_on_an_alias_does_not_accumulate_descriptors() {
    let (_g, dir) = tmp();
    let (first, second) = hard_link_pair(&dir);
    let _held = FileLock::acquire(&first, &zero()).unwrap();
    let before = open_descriptors();
    let options = LockOptions {
        timeout: Duration::from_millis(300),
        poll: Duration::from_millis(5),
        label: String::new(),
    };
    let waited = FileLock::acquire_shared(&second, &options);
    assert!(
        matches!(waited, Err(FsError::LockTimeout { .. })),
        "{waited:?}"
    );
    let after = open_descriptors();
    assert!(
        after <= before + 1,
        "about 60 polls must not park about 60 descriptors: {before} -> {after}"
    );
}

#[test]
fn flock_kind_is_unaffected_by_aliases() {
    // `flock` is per open file description, so the kernel already treats an alias correctly.
    let (_g, dir) = tmp();
    let (first, second) = hard_link_pair(&dir);
    let held = FileLock::try_lock_with(&first, LockKind::Flock, Duration::ZERO).unwrap();
    assert!(FileLock::try_lock_with(&second, LockKind::Flock, Duration::ZERO).is_err());
    drop(held);
    FileLock::try_lock_with(&second, LockKind::Flock, Duration::ZERO).unwrap();
}

#[test]
fn a_lock_file_named_like_a_generated_temp_file_is_refused() {
    let (_g, dir) = tmp();
    let path = dir.join(".x.lock.rktmp-1-2-0");
    let error = FileLock::try_acquire(&path).expect_err("reserved name");
    assert!(matches!(error, FsError::BadName(_)), "{error:?}");
    assert!(!path.exists());
}

#[cfg(feature = "testing")]
mod swap {
    use super::*;
    use rightkit_fs::lock::testing::after_identity_check;

    /// The name `p` is swapped between the pre-open `lstat` (which saw the unheld file Y) and the
    /// `open` (which finds the held file X). The request on `p` is answered by X's entry, and the
    /// descriptor it opened is parked, not closed. Nothing may remember "p looked like Y but was
    /// X": a later request on Y through its own, never-swapped name `q` must take a real lock on
    /// Y (a process that remembered the pairing would grant X's guard and lock nothing on Y).
    #[test]
    fn a_name_swapped_between_the_identity_check_and_the_open_never_lends_one_files_guard_to_another(
    ) {
        let (_g, dir) = tmp();
        let (x, y, p, q) = (
            dir.join("x.lock"),
            dir.join("y.lock"),
            dir.join("p.lock"),
            dir.join("q.lock"),
        );
        std::fs::File::create(&x).unwrap();
        std::fs::File::create(&y).unwrap();
        std::fs::hard_link(&y, &p).unwrap();
        std::fs::hard_link(&y, &q).unwrap();

        let held_x = FileLock::acquire(&x, &zero()).unwrap();
        assert!(!other_process_finds_it_free(&x), "precondition: X is held");
        assert!(other_process_finds_it_free(&q), "precondition: Y is free");

        let swapped = {
            let (hook_p, hook_x) = (p.clone(), x.clone());
            let _hook = after_identity_check(move |path| {
                if path == hook_p {
                    std::fs::remove_file(&hook_p).unwrap();
                    std::fs::hard_link(&hook_x, &hook_p).unwrap();
                }
            });
            FileLock::try_acquire(&p)
        };
        assert!(
            matches!(swapped, Err(FsError::Locked { .. })),
            "the open found X, which is held: {swapped:?}"
        );
        assert!(
            !other_process_finds_it_free(&x),
            "the refused request must not have released X"
        );

        let on_y = FileLock::try_acquire(&q).expect("Y is not held by anyone and must be lockable");
        assert!(
            !other_process_finds_it_free(&q),
            "Y must be really locked, not granted from X's entry"
        );
        drop(on_y);
        assert!(other_process_finds_it_free(&q));
        drop(held_x);
        assert!(other_process_finds_it_free(&x));
    }
}
