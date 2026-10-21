//! RENAME_SWAP contract: exchanges two directories atomically on APFS, errors on a missing path.
#![cfg(target_os = "macos")]

use rightkit_fs::swap::is_swap_unsupported;
use rightkit_fs::swap_paths;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tempdir() -> tempfile::TempDir {
    // The platform temp dir is APFS on macOS, which is what RENAME_SWAP needs.
    tempfile::Builder::new()
        .prefix("swap-test-")
        .tempdir()
        .unwrap()
}

fn two_dirs(root: &Path) -> (PathBuf, PathBuf) {
    let a = root.join("a.app");
    let b = root.join("b.app");
    fs::create_dir_all(a.join("Contents")).unwrap();
    fs::create_dir_all(b.join("Contents")).unwrap();
    fs::write(a.join("Contents/marker"), "old").unwrap();
    fs::write(b.join("Contents/marker"), "new").unwrap();
    (a, b)
}

#[test]
fn swaps_two_directories_and_preserves_inodes() {
    let dir = tempdir();
    let (a, b) = two_dirs(dir.path());
    let (ino_a, ino_b) = (
        fs::metadata(&a).unwrap().ino(),
        fs::metadata(&b).unwrap().ino(),
    );

    swap_paths(&a, &b).expect("swap on APFS");

    assert_eq!(
        fs::read_to_string(a.join("Contents/marker")).unwrap(),
        "new"
    );
    assert_eq!(
        fs::read_to_string(b.join("Contents/marker")).unwrap(),
        "old"
    );
    assert_eq!(fs::metadata(&a).unwrap().ino(), ino_b);
    assert_eq!(fs::metadata(&b).unwrap().ino(), ino_a);

    swap_paths(&a, &b).expect("swap back");
    assert_eq!(
        fs::read_to_string(a.join("Contents/marker")).unwrap(),
        "old"
    );
}

#[test]
fn missing_path_is_a_real_error_not_unsupported() {
    let dir = tempdir();
    let (a, _b) = two_dirs(dir.path());
    let missing = dir.path().join("missing.app");
    let error = swap_paths(&a, &missing).expect_err("missing path must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(!is_swap_unsupported(&error));
    assert_eq!(
        fs::read_to_string(a.join("Contents/marker")).unwrap(),
        "old"
    );
}

#[test]
fn helper_binary_swaps_and_reports_failures() {
    let dir = tempdir();
    let (a, b) = two_dirs(dir.path());
    let bin = env!("CARGO_BIN_EXE_rightkit-swap");

    let ok = Command::new(bin).arg(&a).arg(&b).output().unwrap();
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(
        fs::read_to_string(a.join("Contents/marker")).unwrap(),
        "new"
    );

    let missing = Command::new(bin)
        .arg(&a)
        .arg(dir.path().join("nope"))
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("rightkit-swap:"));

    let usage = Command::new(bin).arg(&a).output().unwrap();
    assert_eq!(usage.status.code(), Some(2));
}
