//! M8-O7b: `sweep_stale_temps_report`.

#![cfg(unix)]

use rightkit_fs::sweep_stale_temps_report;

fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn separates_dead_live_unknown_and_other() {
    let dir = tempfile::tempdir().unwrap();
    let dead = dead_pid();
    let make = |name: String| std::fs::write(dir.path().join(name), b"x").unwrap();
    make(format!(".dead.rktmp-{dead}-1-0"));
    make(format!(".own.rktmp-{}-2-0", std::process::id()));
    make(".unknown.rktmp-0-3-0".to_string());
    make("invoice.rktmp-1-final".to_string());
    // A symlink named like a dead writer's temp is never removed.
    let outside = dir.path().join("outside");
    std::fs::write(&outside, b"keep").unwrap();
    std::os::unix::fs::symlink(&outside, dir.path().join(format!(".link.rktmp-{dead}-4-0")))
        .unwrap();

    let report = sweep_stale_temps_report(dir.path()).unwrap();
    assert!(report.retained_counted);
    assert_eq!(report.removed, 1);
    assert_eq!(report.retained_live, 1, "this process is live");
    assert_eq!(report.retained_unknown, 1, "pid 0 cannot be probed");
    assert_eq!(
        report.retained_other, 1,
        "the symlink is not a regular file"
    );
    assert!(!dir.path().join(format!(".dead.rktmp-{dead}-1-0")).exists());
    assert!(dir.path().join(".unknown.rktmp-0-3-0").exists());
    assert!(dir.path().join("invoice.rktmp-1-final").exists());
    assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
}

#[test]
fn a_missing_directory_is_an_empty_report() {
    let dir = tempfile::tempdir().unwrap();
    let report = sweep_stale_temps_report(&dir.path().join("absent")).unwrap();
    assert_eq!(report.removed, 0);
    assert_eq!(report.retained_unknown, 0);
}
