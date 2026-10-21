//! The temporary-file namespace is reserved. A sweep only ever deletes a *complete generated temp
//! name*, and a public write refuses to create a record with one, so a committed record can never
//! be mistaken for a crash orphan.

use std::time::Duration;

use rightkit_fs::{
    atomic_write, atomic_write_private, rename_exclusive, sweep_stale_temps, FsError, StateDir,
};

#[test]
fn a_record_that_merely_contains_the_marker_survives_a_zero_age_sweep() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "invoice.rktmp-1-final",
        "invoice.rktmp-1",
        "a.rktmp-123-456-789",
        ".hidden.rktmp-1-final",
        "report.rktmp-",
    ] {
        atomic_write(&dir.path().join(name), b"committed").unwrap();
    }
    let removed = sweep_stale_temps(dir.path(), Duration::ZERO).unwrap();
    assert_eq!(removed, 0, "no committed record is a temp file");
    for name in [
        "invoice.rktmp-1-final",
        "invoice.rktmp-1",
        "a.rktmp-123-456-789",
        ".hidden.rktmp-1-final",
        "report.rktmp-",
    ] {
        assert_eq!(
            std::fs::read(dir.path().join(name)).unwrap(),
            b"committed",
            "{name} was deleted by a sweep"
        );
    }
}

#[test]
fn a_public_write_cannot_target_a_generated_temp_name() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        ".state.json.rktmp-4242-7-0",
        ".state.json.rktmp-4242-7-1f4a-3",
        ".x.rktmp-1-2-31",
    ] {
        let target = dir.path().join(name);
        let error = atomic_write(&target, b"x").expect_err("a temp-shaped target is refused");
        assert!(matches!(error, FsError::BadName(_)), "{name}: {error:?}");
        let error = atomic_write_private(&target, b"x").expect_err("refused (private)");
        assert!(matches!(error, FsError::BadName(_)), "{name}: {error:?}");
        assert!(!target.exists());

        let from = dir.path().join("src.bin");
        std::fs::write(&from, b"x").unwrap();
        let error = rename_exclusive(&from, &target).expect_err("refused (exclusive rename)");
        assert!(matches!(error, FsError::BadName(_)), "{name}: {error:?}");
        assert!(from.exists(), "the source is untouched");
    }
    // Nothing was created by the refused writes (not even a temp).
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
    assert_eq!(names.len(), 1, "only src.bin: {names:?}");
}

#[test]
fn a_refused_write_creates_no_parent_directories() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("new/deep/.x.rktmp-1-2-0");
    let options = rightkit_fs::AtomicWriteOptions {
        create_parents: true,
        ..rightkit_fs::AtomicWriteOptions::default()
    };
    let error = rightkit_fs::atomic_write_with_options(&target, options, |f| {
        std::io::Write::write_all(f, b"x")
    })
    .expect_err("refused");
    assert!(matches!(error, FsError::BadName(_)), "{error:?}");
    assert!(!dir.path().join("new").exists());
}

#[test]
fn current_and_unqueryable_writer_temps_survive_a_zero_age_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let pid = std::process::id();
    // Both generated forms: atomic writes (with the nanosecond field) and `StateDir`.
    let atomic_form = dir.path().join(format!(".state.json.rktmp-{pid}-7-1f4a-0"));
    let state_form = dir.path().join(format!(".state.json.rktmp-{pid}-8-1"));
    // PID zero is a complete generated shape but has no process-liveness meaning.
    let unknown_atomic = dir.path().join(".unknown.rktmp-0-9-1f4a-0");
    let unknown_state = dir.path().join(".unknown.rktmp-0-10-0");
    for temp in [&atomic_form, &state_form, &unknown_atomic, &unknown_state] {
        std::fs::write(temp, b"in-flight").unwrap();
    }
    let keeper = dir.path().join("state.json");
    std::fs::write(&keeper, b"committed").unwrap();
    let removed = sweep_stale_temps(dir.path(), Duration::ZERO).unwrap();
    assert_eq!(removed, 0);
    assert!(atomic_form.exists() && state_form.exists());
    assert!(unknown_atomic.exists() && unknown_state.exists());
    assert_eq!(std::fs::read(&keeper).unwrap(), b"committed");
}

#[test]
fn state_dir_refuses_and_never_sweeps_the_reserved_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let state = StateDir::create(&root.join("s")).unwrap();
    state.write("invoice.rktmp-1-final", b"committed").unwrap();
    state.write_new("report.rktmp-9-9", b"committed").unwrap();
    assert!(matches!(
        state.write(".x.rktmp-1-2-0", b"x"),
        Err(FsError::BadName(_))
    ));
    assert!(matches!(
        state.write_new(".x.rktmp-1-2-0", b"x"),
        Err(FsError::BadName(_))
    ));
    assert!(matches!(
        rightkit_fs::state::validate_name(".x.rktmp-1-2-0"),
        Err(FsError::BadName(_))
    ));
    rightkit_fs::state::validate_name("invoice.rktmp-1-final").unwrap();

    assert_eq!(state.sweep_temps(Duration::ZERO).unwrap(), 0);
    assert_eq!(
        state.read("invoice.rktmp-1-final", 100).unwrap(),
        b"committed"
    );
    assert_eq!(state.read("report.rktmp-9-9", 100).unwrap(), b"committed");

    // A current writer's generated name stays even at zero age.
    let current = format!(".state.rktmp-{}-5-0", std::process::id());
    let unknown = ".state.rktmp-0-6-0";
    std::fs::write(root.join("s").join(&current), b"in-flight").unwrap();
    std::fs::write(root.join("s").join(unknown), b"unqueryable").unwrap();
    assert_eq!(state.sweep_temps(Duration::ZERO).unwrap(), 0);
    assert!(state.exists(&current).unwrap());
    assert!(state.exists(unknown).unwrap());
}

#[test]
fn a_long_multibyte_name_writes_and_survives_a_sweep() {
    let dir = tempfile::tempdir().unwrap();
    // 80 two-byte chars: the destination is fine, and the temp prefix is bounded on a char
    // boundary so the generated temp name stays short and parseable.
    let name = format!("{}.json", "\u{e9}".repeat(80));
    let target = dir.path().join(&name);
    atomic_write(&target, b"v").unwrap();
    atomic_write_private(&target, b"w").unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"w");
    assert_eq!(sweep_stale_temps(dir.path(), Duration::ZERO).unwrap(), 0);
    // No temp is left behind by the successful writes.
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
    assert_eq!(names.len(), 1, "{names:?}");
}

#[test]
fn create_private_new_refuses_a_generated_temp_name() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join(".record.rktmp-123-1-0");
    let error = rightkit_fs::private::create_private_new(&target)
        .map(|_| ())
        .expect_err("a temp-shaped name is refused");
    assert!(matches!(error, FsError::BadName(_)), "{error:?}");
    assert!(!target.exists());
    // An ordinary name still works, and a zero-age sweep leaves it alone.
    let kept = dir.path().join("record.log");
    drop(rightkit_fs::private::create_private_new(&kept).unwrap());
    assert_eq!(sweep_stale_temps(dir.path(), Duration::ZERO).unwrap(), 0);
    assert!(kept.exists());
}

/// Win32 drops leading spaces, trailing spaces and periods from the last component, so a name that
/// is not a generated temp name as text can still be created as one. Every creation API that takes
/// a name refuses it. (On other platforms those are ordinary, distinct names; the rule itself is
/// unit-tested for every platform as `temp_name_tests::windows_names_that_normalize_to_a_temp_name_are_reserved`.)
#[cfg(windows)]
#[test]
fn windows_trailing_space_and_period_names_are_refused() {
    use rightkit_fs::create_private_new;
    let dir = tempfile::tempdir().unwrap();
    for name in [
        ".record.rktmp-123-1-0 ",
        ".record.rktmp-123-1-0.",
        ".record.rktmp-123-1-ff-0 . ",
        " .record.rktmp-123-1-0",
        "  .record.rktmp-123-1-ff-0 . ",
    ] {
        let target = dir.path().join(name);
        let error = atomic_write(&target, b"x").expect_err("atomic_write");
        assert!(matches!(error, FsError::BadName(_)), "{name:?}: {error:?}");
        let error = atomic_write_private(&target, b"x").expect_err("atomic_write_private");
        assert!(matches!(error, FsError::BadName(_)), "{name:?}: {error:?}");
        let error = create_private_new(&target).expect_err("create_private_new");
        assert!(matches!(error, FsError::BadName(_)), "{name:?}: {error:?}");
        let from = dir.path().join("src.bin");
        std::fs::write(&from, b"x").unwrap();
        let error = rename_exclusive(&from, &target).expect_err("rename_exclusive");
        assert!(matches!(error, FsError::BadName(_)), "{name:?}: {error:?}");
    }
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
    assert_eq!(names.len(), 1, "only src.bin: {names:?}");
    // And the committed record survives a zero-age sweep.
    let kept = dir.path().join(".record.rktmp-1-final ");
    atomic_write(&kept, b"committed").unwrap();
    assert_eq!(sweep_stale_temps(dir.path(), Duration::ZERO).unwrap(), 0);
}
