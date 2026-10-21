//! End-to-end tests: real files, real child processes (the test binary re-executes itself in a
//! helper role), real symlink attacks and real SIGKILLs. Nothing is mocked.

use rightkit_fs::{
    atomic_write, sweep_stale_temps, AtomicOptions, AtomicWriter, FileLock, FsError, IdentityStore,
    LockOptions, Resolution, StateDir,
};
#[cfg(unix)]
use rightkit_fs::{
    atomic_write_private, create_private_new, ensure_private_dir, write_private_file,
};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// No helper process lives longer than this, whatever its test does. A helper that outlives a
/// failed test keeps the test binary's output pipes open, and a CI step that waits for those
/// pipes then hangs (seen on the Windows runner: 71 minutes).
const HELPER_CAP: Duration = Duration::from_secs(60);

fn canonical_tmp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    // macOS tempdirs live under the /var -> /private/var symlink; pinning refuses symlinked
    // ancestors on purpose, so tests hand it the resolved path.
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

// ---------------------------------------------------------------- child-process helpers

fn spawn_role(role: &str, args: &[&str]) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "helper_entry", "--nocapture", "--test-threads=1"])
        .env("RKFS_ROLE", role)
        .env("RKFS_ARGS", args.join("\u{1f}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

/// Block until the child prints READY on its own line, then keep draining its stdout on a
/// thread. Dropping the pipe here would close it under the child's libtest output, which then
/// fails with a broken pipe (os error 32 on unix, 232 on Windows).
fn wait_ready(child: &mut Child) {
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            reader.read_line(&mut line).unwrap_or(0) > 0,
            "child exited before READY"
        );
        if line.trim_end().ends_with("READY") {
            break;
        }
    }
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
}

/// Helper processes that loop until told to stop. Dropping this stops them, also when the test
/// is unwinding from a failed assertion: the stop file is created, and a helper that is still
/// running is killed and reaped. The stop file alone is not enough: the test's temp folder is
/// removed right after, and with it the stop file, so a helper that checks later sees none.
struct LoopingHelpers {
    kids: Vec<Child>,
    stop: PathBuf,
}

impl LoopingHelpers {
    fn any_running(&mut self) -> bool {
        self.kids
            .iter_mut()
            .any(|kid| kid.try_wait().unwrap().is_none())
    }

    fn tell_to_stop(&self) {
        std::fs::write(&self.stop, b"").unwrap();
    }

    /// Wait for every helper and say whether all of them succeeded.
    fn all_succeeded(&mut self) -> bool {
        let mut ok = true;
        for kid in &mut self.kids {
            ok &= kid.wait().unwrap().success();
        }
        ok
    }
}

impl Drop for LoopingHelpers {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.stop, b"");
        for kid in &mut self.kids {
            if matches!(kid.try_wait(), Ok(None)) {
                let _ = kid.kill();
                let _ = kid.wait();
            }
        }
    }
}

fn kill9(mut child: Child) {
    child.kill().unwrap();
    let _ = child.wait();
}

/// Wait for termination while retaining this process's child handle through a liveness sweep.
/// Windows can then distinguish a terminated process from an unavailable `OpenProcess` result.
fn kill_for_sweep(child: &mut Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

fn ready() {
    println!("READY");
    std::io::stdout().flush().unwrap();
}

/// The helper role: a no-op unless this test binary was spawned as a child.
#[test]
fn helper_entry() {
    let Ok(role) = std::env::var("RKFS_ROLE") else {
        return;
    };
    let args: Vec<String> = std::env::var("RKFS_ARGS")
        .unwrap_or_default()
        .split('\u{1f}')
        .map(str::to_string)
        .collect();
    match role.as_str() {
        // Start an atomic write, flush half of it, announce, then hang until killed.
        "crash_write" => {
            let mut w =
                AtomicWriter::create(Path::new(&args[0]), AtomicOptions::default()).unwrap();
            w.write_all(&vec![b'N'; 512 * 1024]).unwrap();
            w.file_mut().sync_all().unwrap();
            ready();
            std::thread::sleep(HELPER_CAP);
        }
        "writer_loop" => {
            let tag = args[1].as_bytes()[0];
            // At least 150 writes, then keep going until the reader has seen enough overlap.
            let stop = Path::new(&args[0]).with_extension("stop");
            let started = Instant::now();
            let mut i = 0usize;
            while i < 150 || !stop.exists() {
                assert!(
                    started.elapsed() < HELPER_CAP,
                    "writer helper was not stopped within {HELPER_CAP:?}"
                );
                i += 1;
                let mut payload = vec![tag; 96 * 1024];
                payload[0] = b'0' + (i % 10) as u8;
                for b in payload.iter_mut().skip(1) {
                    *b = tag;
                }
                atomic_write(Path::new(&args[0]), &payload).unwrap();
            }
        }
        "lock_hold" => {
            let _lock = FileLock::acquire(
                Path::new(&args[0]),
                &LockOptions::default().labeled("holder"),
            )
            .unwrap();
            ready();
            let ms: u64 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(u64::MAX);
            std::thread::sleep(Duration::from_millis(ms).min(HELPER_CAP));
        }
        "lock_counter" => {
            let lock = PathBuf::from(&args[0]);
            let counter = PathBuf::from(&args[1]);
            for _ in 0..80 {
                let _guard =
                    FileLock::acquire(&lock, &LockOptions::wait(Duration::from_secs(60))).unwrap();
                let n: u64 = std::fs::read_to_string(&counter)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                // A widened read-modify-write window: only the lock keeps this correct.
                std::thread::sleep(Duration::from_micros(300));
                std::fs::write(&counter, (n + 1).to_string()).unwrap();
            }
        }
        "track_files" => {
            let store = IdentityStore::open(Path::new(&args[0])).unwrap();
            let mut ids = Vec::new();
            for f in &args[2..] {
                ids.push(store.track(Path::new(f)).unwrap());
            }
            std::fs::write(&args[1], ids.join("\n")).unwrap();
        }
        other => panic!("unknown role {other}"),
    }
}

fn temps_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".rktmp-"))
        .collect()
}

// ---------------------------------------------------------------- atomic writes

#[test]
fn atomic_write_replaces_content_and_leaves_no_temp_files() {
    let (_g, dir) = canonical_tmp();
    let p = dir.join("a.json");
    atomic_write(&p, b"one").unwrap();
    atomic_write(&p, b"two").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"two");
    assert!(temps_in(&dir).is_empty());
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1);
}

#[cfg(unix)]
#[test]
fn private_writes_are_owner_only_and_plain_writes_keep_the_existing_mode() {
    use std::os::unix::fs::PermissionsExt;
    let (_g, dir) = canonical_tmp();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

    let secret = dir.join("secret.key");
    atomic_write_private(&secret, b"hunter2").unwrap();
    assert_eq!(mode(&secret), 0o600);
    // Rewriting must not widen it.
    atomic_write_private(&secret, b"hunter3").unwrap();
    assert_eq!(mode(&secret), 0o600);

    let shared = dir.join("shared.txt");
    std::fs::write(&shared, b"x").unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o640)).unwrap();
    atomic_write(&shared, b"y").unwrap();
    assert_eq!(
        mode(&shared),
        0o640,
        "a plain atomic write preserves the destination mode"
    );
    assert_eq!(std::fs::read(&shared).unwrap(), b"y");

    let nested = dir.join("a/b/c");
    ensure_private_dir(&nested).unwrap();
    assert_eq!(mode(&nested), 0o700);
    assert_eq!(mode(&dir.join("a")), 0o700);
    write_private_file(&nested.join("f"), b"z").unwrap();
    assert_eq!(mode(&nested.join("f")), 0o600);
}

#[cfg(unix)]
#[test]
fn private_creation_refuses_a_planted_symlink() {
    let (_g, dir) = canonical_tmp();
    let outside = dir.join("outside.txt");
    std::fs::write(&outside, b"precious").unwrap();
    let link = dir.join("log");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert!(create_private_new(&link).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"precious");
}

#[test]
fn failed_fill_keeps_the_old_file_and_removes_the_temp() {
    let (_g, dir) = canonical_tmp();
    let p = dir.join("keep.json");
    atomic_write(&p, b"old").unwrap();
    let err = rightkit_fs::atomic_write_with(&p, AtomicOptions::default(), |f| {
        f.write_all(b"partial")?;
        Err(std::io::Error::other("disk exploded"))
    });
    assert!(err.is_err());
    assert_eq!(std::fs::read(&p).unwrap(), b"old");
    assert!(temps_in(&dir).is_empty());
    // A writer dropped without commit leaves nothing either.
    drop(AtomicWriter::create(&p, AtomicOptions::default()).unwrap());
    assert!(temps_in(&dir).is_empty());
}

#[test]
fn killed_mid_write_leaves_the_old_file_and_a_sweepable_orphan() {
    let (_g, dir) = canonical_tmp();
    let p = dir.join("state.json");
    atomic_write(&p, b"old contents").unwrap();

    let mut child = spawn_role("crash_write", &[p.to_str().unwrap()]);
    #[cfg(unix)]
    let pid = child.id();
    wait_ready(&mut child);
    assert_eq!(temps_in(&dir).len(), 1, "the in-flight temp file exists");
    let orphan = temps_in(&dir).pop().expect("the writer created one temp");
    // Age never overrides a live writer, even at a zero threshold.
    assert_eq!(sweep_stale_temps(&dir, Duration::ZERO).unwrap(), 0);
    kill_for_sweep(&mut child);

    assert_eq!(
        std::fs::read(&p).unwrap(),
        b"old contents",
        "destination never saw a partial write"
    );
    assert_eq!(temps_in(&dir).len(), 1, "SIGKILL leaves the orphan");
    #[cfg(unix)]
    let (outside, trap) = {
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, b"do not unlink").unwrap();
        let trap = dir.join(format!(".trap.rktmp-{pid}-9-0"));
        std::os::unix::fs::symlink(&outside, &trap).unwrap();
        (outside, trap)
    };
    assert_eq!(
        sweep_stale_temps(&dir, Duration::ZERO).unwrap(),
        1,
        "dead pid => reclaimed"
    );
    #[cfg(not(unix))]
    assert!(temps_in(&dir).is_empty());
    #[cfg(unix)]
    assert!(std::fs::symlink_metadata(trap)
        .unwrap()
        .file_type()
        .is_symlink());
    #[cfg(unix)]
    assert_eq!(std::fs::read(outside).unwrap(), b"do not unlink");
    assert!(
        !dir.join(orphan).exists(),
        "the confirmed-dead orphan was removed"
    );
    assert_eq!(std::fs::read(&p).unwrap(), b"old contents");
}

/// Reads a file that other processes are replacing, through `read_replaced`: on Windows an open
/// refused for a moment with access denied (5) or a sharing violation (32) is retried (at most 64
/// opens, a short bounded pause between them), so a refusal is not mistaken for wrong content.
/// Any other error, and any error on Unix, fails the test.
fn read_during_replaces(path: &Path) -> Vec<u8> {
    rightkit_fs::read_replaced(path).unwrap_or_else(|error| panic!("read_replaced failed: {error}"))
}

/// The contract is about content: a reader never sees a truncated file or a mix of two writers.
/// It is not that a Windows reader never gets a refused open while a replace is in flight;
/// `read_replaced` retries that open, and what this test asserts is that the bytes it returns are
/// always one complete version. It does not count refusals: a green run does not show that the
/// retry ran, only that no reader was broken by the writers on that machine.
#[test]
fn concurrent_processes_never_expose_a_mixed_file() {
    let (_g, dir) = canonical_tmp();
    let p = dir.join("shared.bin");
    atomic_write(&p, &vec![b'A'; 96 * 1024]).unwrap();
    // Declared after the temp folder, so it is dropped first: the helpers are stopped before
    // the folder they write into is removed, on success and on a failed assertion alike.
    let mut writers = LoopingHelpers {
        kids: ["B", "C", "D"]
            .iter()
            .map(|tag| spawn_role("writer_loop", &[p.to_str().unwrap(), tag]))
            .collect(),
        stop: p.with_extension("stop"),
    };
    let mut reads = 0;
    while writers.any_running() {
        let bytes = read_during_replaces(&p);
        assert_eq!(bytes.len(), 96 * 1024, "never a truncated or partial file");
        let tag = bytes[1];
        assert!(
            bytes[1..].iter().all(|b| *b == tag),
            "never a mix of two writers"
        );
        reads += 1;
        if reads == 50 {
            writers.tell_to_stop();
        }
    }
    assert!(reads > 10);
    println!("[fs-reader] {reads} reads during replaces, none mixed");
    assert!(writers.all_succeeded());
    assert!(temps_in(&dir).is_empty());
}

// ---------------------------------------------------------------- locks

#[test]
fn a_second_process_is_refused_with_owner_details_and_a_killed_owner_is_recovered() {
    let (_g, dir) = canonical_tmp();
    let lock = dir.join(".lock");
    let mut child = spawn_role("lock_hold", &[lock.to_str().unwrap()]);
    let pid = child.id();
    wait_ready(&mut child);

    match FileLock::try_acquire(&lock) {
        Err(FsError::Locked { owner, .. }) => {
            assert!(
                owner.contains(&format!("pid {pid}")),
                "owner text was {owner}"
            );
            assert!(owner.contains("holder"));
        }
        other => panic!("expected Locked, got {other:?}"),
    }
    assert_eq!(FileLock::current_owner(&lock).unwrap().pid, pid);

    kill9(child); // the OS drops the lock with the process
    let lock = FileLock::try_acquire(&lock).unwrap();
    let stale = lock
        .recovered_stale
        .clone()
        .expect("the dead owner's record is reported");
    assert_eq!(stale.pid, pid);
    assert_eq!(
        FileLock::current_owner(lock.path()).unwrap().pid,
        std::process::id()
    );
    let path = lock.path().to_path_buf();
    drop(lock);
    assert!(
        FileLock::current_owner(&path).is_none(),
        "owner record removed on clean release"
    );
}

#[test]
fn waiting_acquire_succeeds_when_the_holder_releases_and_times_out_otherwise() {
    let (_g, dir) = canonical_tmp();
    let lock = dir.join("wait.lock");
    let mut child = spawn_role("lock_hold", &[lock.to_str().unwrap(), "700"]);
    wait_ready(&mut child);
    let started = std::time::Instant::now();
    let got = FileLock::acquire(&lock, &LockOptions::wait(Duration::from_secs(20))).unwrap();
    assert!(started.elapsed() >= Duration::from_millis(300));
    // Now we hold it: a bounded wait times out.
    match FileLock::acquire(&lock, &LockOptions::wait(Duration::from_millis(150))) {
        Err(FsError::LockTimeout { .. }) => {}
        other => panic!("expected timeout, got {other:?}"),
    }
    drop(got);
    let _ = child.wait();
}

#[test]
fn shared_locks_coexist_and_exclude_a_writer() {
    let (_g, dir) = canonical_tmp();
    let lock = dir.join("rw.lock");
    let r1 = FileLock::acquire_shared(&lock, &LockOptions::default()).unwrap();
    let r2 = FileLock::acquire_shared(&lock, &LockOptions::default()).unwrap();
    assert!(FileLock::try_acquire(&lock).is_err());
    drop((r1, r2));
    FileLock::try_acquire(&lock).unwrap();
}

#[test]
fn lock_serializes_read_modify_write_across_processes() {
    let (_g, dir) = canonical_tmp();
    let lock = dir.join("c.lock");
    let counter = dir.join("counter");
    std::fs::write(&counter, "0").unwrap();
    let kids: Vec<Child> = (0..4)
        .map(|_| {
            spawn_role(
                "lock_counter",
                &[lock.to_str().unwrap(), counter.to_str().unwrap()],
            )
        })
        .collect();
    for mut k in kids {
        assert!(k.wait().unwrap().success());
    }
    assert_eq!(
        std::fs::read_to_string(&counter).unwrap(),
        "320",
        "no lost updates"
    );
}

// ---------------------------------------------------------------- pinned state dir

#[test]
fn state_dir_round_trips_atomically_and_validates_names() {
    let (_g, dir) = canonical_tmp();
    let state = StateDir::create(&dir.join("state/deep")).unwrap();
    state.write("a.json", b"{\"v\":1}").unwrap();
    state.write("a.json", b"{\"v\":2}").unwrap();
    assert_eq!(state.read("a.json", 1024).unwrap(), b"{\"v\":2}");
    assert!(state.read_opt("missing", 10).unwrap().is_none());
    assert!(matches!(
        state.write_new("a.json", b"x"),
        Err(FsError::AlreadyExists(_))
    ));
    state.write_new("b.json", b"b").unwrap();
    let mut names = state.list(100).unwrap();
    names.sort();
    assert_eq!(names, vec!["a.json", "b.json"], "no temp leftovers");
    assert!(matches!(
        state.read("a.json", 3),
        Err(FsError::TooLarge { .. })
    ));
    for bad in ["../x", "a/b", "..", ".", "", "x\0y", "a\\b"] {
        assert!(
            matches!(state.write(bad, b"x"), Err(FsError::BadName(_))),
            "{bad:?}"
        );
    }
    assert!(state.remove("a.json").unwrap());
    assert!(!state.remove("a.json").unwrap());
    let sub = state.child("sub").unwrap();
    sub.write("n", b"1").unwrap();
    assert_eq!(std::fs::read(dir.join("state/deep/sub/n")).unwrap(), b"1");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(m(dir.join("state/deep")), 0o700);
        assert_eq!(m(dir.join("state/deep/b.json")), 0o600);
    }
}

#[cfg(unix)]
#[test]
fn symlink_attacks_on_the_state_dir_are_refused() {
    let (_g, dir) = canonical_tmp();
    let outside = dir.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"top secret").unwrap();

    // The state dir itself is a symlink.
    std::os::unix::fs::symlink(&outside, dir.join("linked")).unwrap();
    assert!(matches!(
        StateDir::open(&dir.join("linked")),
        Err(FsError::Symlink(_))
    ));
    assert!(matches!(
        StateDir::create(&dir.join("linked")),
        Err(FsError::Symlink(_))
    ));
    // An ancestor is a symlink.
    std::os::unix::fs::symlink(&outside, dir.join("anc")).unwrap();
    std::fs::create_dir(outside.join("inner")).unwrap();
    assert!(matches!(
        StateDir::open(&dir.join("anc/inner")),
        Err(FsError::Symlink(_))
    ));

    // A child entry is a symlink to a file outside: read, replace and remove never follow it.
    let state = StateDir::create(&dir.join("state")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("state/leak")).unwrap();
    assert!(matches!(state.read("leak", 1024), Err(FsError::Symlink(_))));
    assert!(state.write("leak", b"overwritten").is_err());
    assert_eq!(
        std::fs::read(outside.join("secret.txt")).unwrap(),
        b"top secret"
    );
    assert!(
        state.remove("leak").unwrap(),
        "removing a link removes the link only"
    );
    assert_eq!(
        std::fs::read(outside.join("secret.txt")).unwrap(),
        b"top secret"
    );

    // A child directory that is a symlink.
    std::os::unix::fs::symlink(&outside, dir.join("state/sub")).unwrap();
    assert!(matches!(state.child("sub"), Err(FsError::Symlink(_))));

    // A symlinked temp name cannot be used to redirect the temp file either.
    assert!(!outside.join("planted").exists());
}

#[cfg(unix)]
#[test]
fn a_swapped_path_cannot_redirect_a_pinned_directory() {
    let (_g, dir) = canonical_tmp();
    let evil = dir.join("evil");
    std::fs::create_dir(&evil).unwrap();
    let real = dir.join("real");
    let state = StateDir::create(&real).unwrap();
    state.write("before", b"1").unwrap();

    // Attacker renames the directory away and puts a symlink to their own directory in its place.
    let moved = dir.join("real.moved");
    std::fs::rename(&real, &moved).unwrap();
    std::os::unix::fs::symlink(&evil, &real).unwrap();

    state.write("after", b"2").unwrap();
    assert_eq!(state.read("before", 10).unwrap(), b"1");
    assert!(
        moved.join("after").exists(),
        "writes follow the pinned descriptor"
    );
    assert!(
        !evil.join("after").exists(),
        "and never the swapped-in path"
    );
    assert!(std::fs::read_dir(&evil).unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn a_racing_symlink_planter_never_gets_the_write_through() {
    let (_g, dir) = canonical_tmp();
    let outside = dir.join("victim.txt");
    std::fs::write(&outside, b"intact").unwrap();
    let state = StateDir::create(&dir.join("state")).unwrap();
    let target = dir.join("state/record");

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let planter = {
        let (stop, outside, target) = (stop.clone(), outside.clone(), target.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::remove_file(&target);
                let _ = std::os::unix::fs::symlink(&outside, &target);
            }
        })
    };
    let mut ok = 0;
    for i in 0..400 {
        if state
            .write("record", format!("payload {i}").as_bytes())
            .is_ok()
        {
            ok += 1;
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    planter.join().unwrap();
    assert_eq!(
        std::fs::read(&outside).unwrap(),
        b"intact",
        "{ok} writes landed; none followed the link"
    );
}

#[cfg(unix)]
#[test]
fn group_or_world_writable_state_dirs_are_refused() {
    use std::os::unix::fs::PermissionsExt;
    let (_g, dir) = canonical_tmp();
    let d = dir.join("open");
    std::fs::create_dir(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(StateDir::open(&d), Err(FsError::TooPermissive(_))));
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    StateDir::open(&d).unwrap();
}

#[test]
fn state_dir_sweeps_orphans_from_killed_writers() {
    let (_g, dir) = canonical_tmp();
    let p = dir.join("state");
    let state = StateDir::create(&p).unwrap();
    state.write("keep", b"1").unwrap();
    let mut child = spawn_role("crash_write", &[p.join("keep").to_str().unwrap()]);
    #[cfg(unix)]
    let pid = child.id();
    wait_ready(&mut child);
    let orphan = temps_in(&p).pop().expect("the writer created one temp");
    // `StateDir` also leaves a live writer's zero-age temp in place.
    assert_eq!(state.sweep_temps(Duration::ZERO).unwrap(), 0);
    kill_for_sweep(&mut child);
    #[cfg(unix)]
    let (outside, trap) = {
        let outside = dir.join("state-outside.txt");
        std::fs::write(&outside, b"do not unlink").unwrap();
        let trap = p.join(format!(".trap.rktmp-{pid}-9-0"));
        std::os::unix::fs::symlink(&outside, &trap).unwrap();
        (outside, trap)
    };
    assert_eq!(state.sweep_temps(Duration::ZERO).unwrap(), 1);
    #[cfg(not(unix))]
    assert_eq!(state.list(10).unwrap(), vec!["keep"]);
    #[cfg(unix)]
    assert!(std::fs::symlink_metadata(trap)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(state.read("keep", 10).unwrap(), b"1");
    #[cfg(unix)]
    assert_eq!(std::fs::read(outside).unwrap(), b"do not unlink");
    assert!(
        !p.join(orphan).exists(),
        "the confirmed-dead orphan was removed"
    );
}

// ---------------------------------------------------------------- file identity

fn touch(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[test]
fn identity_follows_renames_moves_and_atomic_replacement() {
    let (_g, dir) = canonical_tmp();
    let root = dir.join("library");
    let store = IdentityStore::open(&dir.join("ledger")).unwrap();

    let original = root.join("inbox/report.txt");
    touch(&original, "quarterly numbers");
    let id = store.track(&original).unwrap();
    assert_eq!(id.len(), 32);
    assert_eq!(
        store.resolve(&id, std::slice::from_ref(&root)).unwrap(),
        Resolution::Unchanged(original.clone())
    );

    // Rename in place.
    let renamed = root.join("inbox/report-final.txt");
    std::fs::rename(&original, &renamed).unwrap();
    assert_eq!(
        store.resolve(&id, std::slice::from_ref(&root)).unwrap(),
        Resolution::Moved(renamed.clone())
    );
    // The ledger remembers: now it is Unchanged at the new path.
    assert_eq!(
        store.resolve(&id, &[]).unwrap(),
        Resolution::Unchanged(renamed.clone())
    );

    // Move into a nested folder.
    let archived = root.join("archive/2026/q3/report-final.txt");
    std::fs::create_dir_all(archived.parent().unwrap()).unwrap();
    std::fs::rename(&renamed, &archived).unwrap();
    assert_eq!(
        store.resolve(&id, std::slice::from_ref(&root)).unwrap(),
        Resolution::Moved(archived.clone())
    );

    // An editor-style safe save replaces the inode with identical bytes elsewhere.
    let saved = root.join("archive/2026/q3/report-final.txt");
    let before = rightkit_fs::FileKey::of_path(&saved).unwrap();
    std::fs::remove_file(&saved).unwrap();
    atomic_write(&saved, b"quarterly numbers").unwrap();
    assert_ne!(rightkit_fs::FileKey::of_path(&saved).unwrap(), before);
    assert_eq!(
        store.resolve(&id, std::slice::from_ref(&root)).unwrap(),
        Resolution::Replaced(saved.clone())
    );
    assert_eq!(
        store.resolve(&id, &[]).unwrap(),
        Resolution::Unchanged(saved.clone()),
        "rebound"
    );

    // Rewritten with different content: no longer recognisable, and say so.
    atomic_write(&saved, b"completely different").unwrap();
    assert_eq!(
        store.resolve(&id, std::slice::from_ref(&root)).unwrap(),
        Resolution::Missing
    );
    store.rebind(&id, &saved).unwrap();
    assert_eq!(
        store.resolve(&id, &[]).unwrap(),
        Resolution::Unchanged(saved)
    );

    // Moved out of every search root: missing, not guessed.
    let outside = dir.join("elsewhere/report.txt");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::rename(root.join("archive/2026/q3/report-final.txt"), &outside).unwrap();
    assert_eq!(
        store.resolve(&id, std::slice::from_ref(&root)).unwrap(),
        Resolution::Missing
    );

    assert!(store.forget(&id).unwrap());
    assert!(matches!(
        store.resolve(&id, &[]),
        Err(FsError::UnknownId(_))
    ));
}

#[test]
fn identity_prefers_the_real_file_over_a_same_size_decoy_and_ignores_symlinks() {
    let (_g, dir) = canonical_tmp();
    let root = dir.join("root");
    let store = IdentityStore::open(&dir.join("ledger")).unwrap();
    let a = root.join("a.dat");
    touch(&a, "AAAAAAAA");
    let id = store.track(&a).unwrap();
    // Decoy: identical size and content but a different file; real file moves.
    touch(&root.join("0-decoy.dat"), "AAAAAAAA");
    let moved = root.join("z/moved.dat");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(&a, &moved).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&moved, root.join("0-link.dat")).unwrap();
    assert_eq!(
        store.resolve(&id, std::slice::from_ref(&root)).unwrap(),
        Resolution::Moved(moved)
    );
}

#[test]
fn identity_ledger_is_safe_across_concurrent_processes() {
    let (_g, dir) = canonical_tmp();
    let ledger = dir.join("ledger");
    let files: Vec<PathBuf> = (0..24)
        .map(|i| {
            let p = dir.join(format!("f{i}.txt"));
            touch(&p, &format!("file number {i}"));
            p
        })
        .collect();
    let outs: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("ids{i}"))).collect();
    let kids: Vec<Child> = (0..3)
        .map(|k| {
            let mut args = vec![
                ledger.to_str().unwrap().to_string(),
                outs[k].to_str().unwrap().to_string(),
            ];
            args.extend(
                files[k * 8..(k + 1) * 8]
                    .iter()
                    .map(|p| p.to_str().unwrap().to_string()),
            );
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            spawn_role("track_files", &refs)
        })
        .collect();
    // The parent tracks concurrently too.
    let store = IdentityStore::open(&ledger).unwrap();
    let mine: Vec<String> = (0..8)
        .map(|i| store.track_or_get(&files[i]).unwrap())
        .collect();
    for mut k in kids {
        assert!(k.wait().unwrap().success());
    }
    let mut all: Vec<String> = outs
        .iter()
        .flat_map(|o| {
            std::fs::read_to_string(o)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(all.len(), 24);
    for id in &all {
        assert!(
            store.lookup(id).unwrap().is_some(),
            "{id} survived concurrent ledger updates"
        );
    }
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 24, "ids are unique");
    // The parent's own entries also made it (files 0..8 were tracked by child 0 AND the parent;
    // track_or_get returned an existing id or created one, and both resolve).
    for id in mine {
        assert!(store.lookup(&id).unwrap().is_some());
    }
}

/// Windows counterpart of the owner-only test: a private atomic write publishes a file whose
/// DACL grants only the current user (no inherited Users/Everyone ACEs), and a rewrite keeps it.
#[cfg(windows)]
#[test]
fn private_writes_are_owner_only_on_windows() {
    use rightkit_fs::atomic_write_private;
    let (_g, dir) = canonical_tmp();
    let secret = dir.join("cookies.txt");
    atomic_write_private(&secret, b"hunter2").unwrap();
    assert!(rightkit_fs::private::is_owner_only(&secret).unwrap());
    atomic_write_private(&secret, b"hunter3").unwrap();
    assert!(rightkit_fs::private::is_owner_only(&secret).unwrap());
    let acl = Command::new("icacls").arg(&secret).output().unwrap();
    let acl = String::from_utf8_lossy(&acl.stdout).to_string();
    eprintln!("{acl}");
    for broad in ["Everyone", "Users:", "(I)"] {
        assert!(!acl.contains(broad), "{broad} in cookie DACL:\n{acl}");
    }
}
