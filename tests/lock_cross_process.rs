#![cfg(unix)]
//! Locks must exclude other processes, share between readers, and die with their holder.
//!
//! The holder is this test binary re-executed with an environment variable, so the lock is
//! taken by a real second process rather than a second descriptor in this one.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(feature = "json")]
use rightkit_fs::probe_owner;
use rightkit_fs::{FileLock, LockKind, LockOptions};

const HOLDER_ENV: &str = "RIGHTKIT_FS_TEST_LOCK_HOLDER";

/// Runs in the re-executed child: take the lock named by the env var, announce it, then hold
/// until stdin closes or says anything.
#[test]
fn holder_entry_point() {
    let Ok(spec) = std::env::var(HOLDER_ENV) else {
        return;
    };
    let (mode, path) = spec.split_once(':').unwrap();
    let options = LockOptions::default();
    let path = Path::new(path);
    let _lock = match mode {
        "shared" => FileLock::acquire_shared(path, &options).unwrap(),
        "flock" => FileLock::try_lock_with(path, LockKind::Flock, Duration::ZERO).unwrap(),
        "flock-shared" => FileLock::acquire_shared_kind(path, LockKind::Flock, &options).unwrap(),
        "fcntl" => FileLock::try_lock_with(path, LockKind::Fcntl, Duration::ZERO).unwrap(),
        _ => FileLock::try_acquire(path).unwrap(),
    };
    println!("HELD");
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

fn spawn_holder(mode: &str, path: &Path) -> Child {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "holder_entry_point",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HOLDER_ENV, format!("{mode}:{}", path.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("HELD") {
        line.clear();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "holder exited early"
        );
    }
    // Keep draining so the holder never blocks on a full pipe.
    std::thread::spawn(move || {
        let mut sink = String::new();
        while reader.read_line(&mut sink).unwrap_or(0) > 0 {
            sink.clear();
        }
    });
    child
}

#[test]
fn exclusive_holder_in_another_process_blocks_every_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.lock");
    let mut holder = spawn_holder("exclusive", &path);

    assert!(FileLock::try_acquire(&path).is_err());
    assert!(FileLock::acquire_shared(&path, &LockOptions::default()).is_err());

    drop(holder.stdin.take());
    holder.wait().unwrap();
    FileLock::try_acquire(&path).expect("free after the holder exits");
}

#[test]
fn shared_holder_in_another_process_admits_readers_and_blocks_writers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.lock");
    let mut holder = spawn_holder("shared", &path);

    let reader = FileLock::acquire_shared(&path, &LockOptions::default())
        .expect("shared coexists across processes");
    assert!(FileLock::try_acquire(&path).is_err());
    drop(reader);
    assert!(
        FileLock::try_acquire(&path).is_err(),
        "dropping our descriptor must not release the other process's lock"
    );

    drop(holder.stdin.take());
    holder.wait().unwrap();
    FileLock::try_acquire(&path).expect("free after the holder exits");
}

#[test]
fn killed_holder_leaves_a_recoverable_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("k.lock");
    let mut holder = spawn_holder("exclusive", &path);
    assert!(FileLock::try_acquire(&path).is_err());

    holder.kill().unwrap();
    holder.wait().unwrap();
    FileLock::try_acquire(&path).expect("kernel releases the record lock when the owner dies");
}

#[test]
fn waiting_acquire_succeeds_once_the_other_process_releases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.lock");
    let mut holder = spawn_holder("exclusive", &path);

    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(holder.stdin.take());
        holder.wait().unwrap();
    });
    let started = Instant::now();
    let lock = FileLock::acquire(&path, &LockOptions::wait(Duration::from_secs(10)))
        .expect("waiting acquire must outlast the holder");
    assert!(started.elapsed() >= Duration::from_millis(200));
    drop(lock);
    releaser.join().unwrap();
}

#[test]
fn flock_exclusive_holder_in_another_process_blocks_flock_callers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.lock");
    let mut holder = spawn_holder("flock", &path);

    assert!(FileLock::try_lock_with(&path, LockKind::Flock, Duration::ZERO).is_err());
    assert!(
        FileLock::acquire_shared_kind(&path, LockKind::Flock, &LockOptions::default()).is_err(),
        "a flock writer must exclude shared flock readers"
    );

    drop(holder.stdin.take());
    holder.wait().unwrap();
    FileLock::try_lock_with(&path, LockKind::Flock, Duration::ZERO)
        .expect("free after the flock holder exits");
}

#[test]
fn flock_shared_holder_in_another_process_admits_readers_and_blocks_writers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.lock");
    let mut holder = spawn_holder("flock-shared", &path);

    let reader = FileLock::acquire_shared_kind(&path, LockKind::Flock, &LockOptions::default())
        .expect("shared flock coexists across processes");
    assert!(FileLock::try_lock_with(&path, LockKind::Flock, Duration::ZERO).is_err());
    drop(reader);
    assert!(
        FileLock::try_lock_with(&path, LockKind::Flock, Duration::ZERO).is_err(),
        "dropping our descriptor must not release the other process's flock"
    );

    drop(holder.stdin.take());
    holder.wait().unwrap();
    FileLock::try_lock_with(&path, LockKind::Flock, Duration::ZERO)
        .expect("free after the flock holder exits");
}

#[test]
fn killed_flock_holder_releases_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h.lock");
    let mut holder = spawn_holder("flock", &path);
    assert!(FileLock::try_lock_with(&path, LockKind::Flock, Duration::ZERO).is_err());

    holder.kill().unwrap();
    holder.wait().unwrap();
    FileLock::try_lock_with(&path, LockKind::Flock, Duration::ZERO)
        .expect("the kernel releases a flock when its last descriptor closes");
}

#[test]
fn fcntl_exclusive_holder_in_another_process_blocks_explicit_fcntl_callers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.lock");
    let mut holder = spawn_holder("fcntl", &path);

    assert!(FileLock::try_lock_with(&path, LockKind::Fcntl, Duration::ZERO).is_err());
    assert!(FileLock::acquire_shared(&path, &LockOptions::default()).is_err());

    drop(holder.stdin.take());
    holder.wait().unwrap();
    FileLock::try_lock_with(&path, LockKind::Fcntl, Duration::ZERO)
        .expect("free after the fcntl holder exits");
}

// Owner records are JSON: without the `json` feature `probe_owner` always returns `None`.
#[cfg(feature = "json")]
#[test]
fn probe_owner_reads_a_live_holder_in_another_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("o.lock");
    let mut holder = spawn_holder("exclusive", &path);

    let owner = probe_owner(&path).expect("the live holder's record is readable");
    assert_eq!(owner.pid, holder.id());
    assert!(
        FileLock::try_acquire(&path).is_err(),
        "probing must not disturb the holder"
    );

    drop(holder.stdin.take());
    holder.wait().unwrap();
    assert!(probe_owner(&path).is_none(), "record removed on clean exit");
}
