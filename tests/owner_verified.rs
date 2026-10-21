//! M8-O9: `copy_verified`.

use rightkit_fs::digest::sha256_bytes;
use rightkit_fs::{copy_verified, AtomicWriteOptions, FsError};

fn entries(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn copies_hashes_and_publishes_across_several_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let body: Vec<u8> = (0..(64 * 1024 * 2 + 31)).map(|i| (i % 251) as u8).collect();
    let src = dir.path().join("src.bin");
    std::fs::write(&src, &body).unwrap();
    let dest = dir.path().join("dest.bin");
    let copied = copy_verified(&src, &dest, AtomicWriteOptions::default(), None).unwrap();
    assert_eq!(copied.sha256, sha256_bytes(&body));
    assert_eq!(copied.bytes, body.len() as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert_eq!(entries(dir.path()), vec!["dest.bin", "src.bin"]);
}

#[test]
fn an_expected_digest_in_any_case_is_accepted_and_an_empty_file_copies() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s");
    std::fs::write(&src, b"abc").unwrap();
    let expected = sha256_bytes(b"abc").to_ascii_uppercase();
    copy_verified(
        &src,
        &dir.path().join("d"),
        AtomicWriteOptions::default(),
        Some(&expected),
    )
    .unwrap();
    let empty = dir.path().join("e");
    std::fs::write(&empty, b"").unwrap();
    let copied = copy_verified(
        &empty,
        &dir.path().join("e2"),
        AtomicWriteOptions::default(),
        None,
    )
    .unwrap();
    assert_eq!(copied.bytes, 0);
}

#[test]
fn a_wrong_digest_publishes_nothing_keeps_the_old_destination_and_leaves_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s");
    std::fs::write(&src, b"new").unwrap();
    let dest = dir.path().join("d");
    std::fs::write(&dest, b"old").unwrap();
    let wrong = sha256_bytes(b"something else");
    let error =
        copy_verified(&src, &dest, AtomicWriteOptions::default(), Some(&wrong)).unwrap_err();
    assert!(matches!(error, FsError::DigestMismatch { .. }), "{error:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    assert_eq!(entries(dir.path()), vec!["d", "s"]);
}

#[test]
fn a_missing_source_a_directory_source_and_a_missing_parent_fail_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let options = AtomicWriteOptions::default();
    let error = copy_verified(
        &dir.path().join("absent"),
        &dir.path().join("d"),
        options,
        None,
    )
    .unwrap_err();
    assert!(error.is_not_found(), "{error:?}");
    let error = copy_verified(dir.path(), &dir.path().join("d"), options, None).unwrap_err();
    assert!(matches!(error, FsError::NotAFile(_)), "{error:?}");
    let src = dir.path().join("s");
    std::fs::write(&src, b"x").unwrap();
    assert!(copy_verified(&src, &dir.path().join("no").join("d"), options, None).is_err());
    assert!(!dir.path().join("d").exists());
}

#[cfg(unix)]
#[test]
fn a_symlink_source_is_refused_and_private_copies_are_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::write(&real, b"secret").unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let error = copy_verified(
        &link,
        &dir.path().join("d"),
        AtomicWriteOptions::default(),
        None,
    )
    .unwrap_err();
    assert!(matches!(error, FsError::Symlink(_)), "{error:?}");
    assert!(!dir.path().join("d").exists());

    let options = AtomicWriteOptions {
        private: true,
        ..AtomicWriteOptions::default()
    };
    let private = dir.path().join("private");
    copy_verified(&real, &private, options, None).unwrap();
    let mode = std::fs::metadata(&private).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn sha256_progress_wrapper_keeps_existing_result_shape() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    let dest = d.path().join("dest");
    std::fs::write(&src, b"abc").unwrap();
    let mut events = Vec::new();
    let result = rightkit_fs::copy_verified_with_progress(
        &src,
        &dest,
        AtomicWriteOptions::default(),
        Some(&sha256_bytes(b"abc")),
        |n, total| events.push((n, total)),
    )
    .unwrap();
    assert_eq!(result.sha256, sha256_bytes(b"abc"));
    assert_eq!(result.bytes, 3);
    assert_eq!(events, [(0, 3), (3, 3)]);
}
