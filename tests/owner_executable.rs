//! M8-O10: `write_private_executable`.

use rightkit_fs::{write_private_executable, FsError};

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[cfg(unix)]
#[test]
fn writes_a_0700_file_and_replaces_a_broader_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("askpass.sh");
    write_private_executable(&path, b"#!/bin/sh\necho secret\n").unwrap();
    assert_eq!(mode(&path), 0o700);
    assert_eq!(std::fs::read(&path).unwrap(), b"#!/bin/sh\necho secret\n");
    assert!(rightkit_fs::private::is_owner_only(&path).unwrap());

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_private_executable(&path, b"#!/bin/sh\n").unwrap();
    assert_eq!(mode(&path), 0o700, "a replaced file ends owner-only");
    assert_eq!(std::fs::read(&path).unwrap(), b"#!/bin/sh\n");
}

#[cfg(unix)]
#[test]
fn the_written_script_can_be_executed_by_its_owner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.sh");
    write_private_executable(&path, b"#!/bin/sh\nexit 7\n").unwrap();
    let status = std::process::Command::new(&path).status().unwrap();
    assert_eq!(status.code(), Some(7));
}

#[test]
fn leaves_no_temp_file_and_refuses_a_generated_temp_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("helper");
    write_private_executable(&path, b"x").unwrap();
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["helper".to_string()]);

    let reserved = dir.path().join(".helper.rktmp-123-1-0");
    let error = write_private_executable(&reserved, b"x").unwrap_err();
    assert!(matches!(error, FsError::BadName(_)), "{error:?}");
    assert!(!reserved.exists());
}

#[test]
fn a_missing_parent_fails_without_creating_anything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent").join("helper");
    assert!(write_private_executable(&path, b"x").is_err());
    assert!(!dir.path().join("absent").exists());
}
