//! G8: BLAKE3 identity, byte progress & checked publication through public APIs.
use rightkit_fs::digest::{
    blake3_bytes, blake3_file, blake3_file_with_len, blake3_file_with_progress,
};
use rightkit_fs::{copy_verified_with_digest, AtomicWriteOptions, DigestAlgorithm, FsError};

fn copy(
    src: &std::path::Path,
    dest: &std::path::Path,
    expected: Option<&str>,
    progress: impl FnMut(u64, u64),
) -> rightkit_fs::Result<rightkit_fs::VerifiedDigestCopy> {
    copy_verified_with_digest(
        src,
        dest,
        AtomicWriteOptions::default(),
        DigestAlgorithm::Blake3,
        expected,
        progress,
    )
}

#[test]
fn streaming_hash_and_copy_report_real_chunk_progress() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    let dest = d.path().join("dest");
    let body: Vec<_> = (0..(2 * 65536 + 17)).map(|i| (i % 251) as u8).collect();
    let total = body.len() as u64;
    std::fs::write(&src, &body).unwrap();
    let expected = blake3_bytes(&body);
    let mut hashing = Vec::new();
    assert_eq!(
        blake3_file_with_progress(&src, |n, t| hashing.push((n, t))).unwrap(),
        (expected.clone(), total)
    );
    assert_eq!(
        hashing,
        [(0, total), (65536, total), (131072, total), (total, total)]
    );
    assert_eq!(blake3_file(&src).unwrap(), expected);
    assert_eq!(
        blake3_file_with_len(&src).unwrap(),
        (expected.clone(), total)
    );
    let mut copying = Vec::new();
    let copied = copy(&src, &dest, Some(&expected.to_ascii_uppercase()), |n, t| {
        copying.push((n, t))
    })
    .unwrap();
    assert_eq!(copying, hashing);
    assert_eq!(copied.algorithm, DigestAlgorithm::Blake3);
    assert_eq!(copied.digest, expected);
    assert_eq!(copied.bytes, total);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 2);
}

#[test]
fn empty_files_report_zero_once_and_missing_files_do_not_report() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    let dest = d.path().join("dest");
    std::fs::write(&src, b"").unwrap();
    let mut events = Vec::new();
    assert_eq!(
        blake3_file_with_progress(&src, |n, t| events.push((n, t))).unwrap(),
        (blake3_bytes(b""), 0)
    );
    assert_eq!(events, [(0, 0)]);
    events.clear();
    assert_eq!(
        copy(&src, &dest, None, |n, t| events.push((n, t)))
            .unwrap()
            .bytes,
        0
    );
    assert_eq!(events, [(0, 0)]);
    events.clear();
    assert!(copy(&d.path().join("missing"), &dest, None, |n, t| events
        .push((n, t)))
    .is_err());
    assert!(
        blake3_file_with_progress(&d.path().join("missing"), |n, t| events.push((n, t))).is_err()
    );
    assert!(events.is_empty());
}

#[test]
fn mismatch_preserves_destination_and_removes_temp() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    let dest = d.path().join("dest");
    std::fs::write(&src, b"new").unwrap();
    std::fs::write(&dest, b"old").unwrap();
    let error = copy(&src, &dest, Some(&blake3_bytes(b"wrong")), |_, _| {}).unwrap_err();
    assert!(matches!(error, FsError::DigestMismatch { path, .. } if path == src));
    assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 2);
}

#[test]
fn changed_source_length_is_refused_even_without_expected_digest() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    let dest = d.path().join("dest");
    std::fs::write(&src, vec![1; 65536 * 2]).unwrap();
    std::fs::write(&dest, b"old").unwrap();
    let error = copy(&src, &dest, None, |n, _| {
        if n == 65536 {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&src)
                .unwrap()
                .set_len(n)
                .unwrap();
        }
    })
    .unwrap_err();
    assert!(matches!(error, FsError::Corrupt { .. }));
    assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 2);
}

#[test]
fn written_temp_is_rehashed_before_publication() {
    use std::io::{Seek, Write};
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    let dest = d.path().join("dest");
    std::fs::write(&src, b"new").unwrap();
    std::fs::write(&dest, b"old").unwrap();
    let error = copy(&src, &dest, None, |n, _| {
        if n != 0 {
            let temp = std::fs::read_dir(d.path())
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p != &src && p != &dest)
                .unwrap();
            let mut f = std::fs::OpenOptions::new().write(true).open(temp).unwrap();
            f.seek(std::io::SeekFrom::Start(0)).unwrap();
            f.write_all(b"bad").unwrap();
        }
    })
    .unwrap_err();
    assert!(matches!(error, FsError::DigestMismatch { path, .. } if path == dest));
    assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 2);
}

#[test]
fn callback_panic_cleans_up_before_publication() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    let dest = d.path().join("dest");
    std::fs::write(&src, b"new").unwrap();
    std::fs::write(&dest, b"old").unwrap();
    let result = std::panic::catch_unwind(|| {
        copy(&src, &dest, None, |n, _| {
            if n != 0 {
                panic!("callback panic");
            }
        })
    });
    assert!(result.is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 2);
}

#[cfg(unix)]
#[test]
fn symlink_source_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    let link = d.path().join("link");
    let dest = d.path().join("dest");
    std::fs::write(&src, b"new").unwrap();
    std::os::unix::fs::symlink(&src, &link).unwrap();
    assert!(matches!(
        copy(&link, &dest, None, |_, _| {}).unwrap_err(),
        FsError::Symlink(_)
    ));
    assert!(!dest.exists());
}
