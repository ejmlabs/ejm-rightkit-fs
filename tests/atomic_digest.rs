//! Incremental SHA-256 and the digest of an atomic write, against the one-shot function.

#![cfg(feature = "digest")]

use std::io::Write;

use rightkit_fs::atomic_write_with_digest;
use rightkit_fs::digest::{sha256_bytes, Sha256Hasher};
use rightkit_fs::{AtomicOptions, AtomicWriteOptions, AtomicWriter};

#[test]
fn the_incremental_hasher_matches_the_one_shot_digest_however_the_input_is_split() {
    let data: Vec<u8> = (0..200_000u32).map(|n| (n % 251) as u8).collect();
    for chunk in [1usize, 7, 4096, 65_537, data.len()] {
        let mut hasher = Sha256Hasher::new();
        for piece in data.chunks(chunk) {
            hasher.update(piece);
        }
        assert_eq!(hasher.finalize(), sha256_bytes(&data), "chunk size {chunk}");
    }
    // The empty input and the io::Write form.
    assert_eq!(Sha256Hasher::new().finalize(), sha256_bytes(b""));
    let mut hasher = Sha256Hasher::new();
    hasher.write_all(b"abc").unwrap();
    assert_eq!(hasher.clone().finalize_bytes().len(), 32);
    assert_eq!(
        hasher.finalize(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn an_atomic_write_returns_the_digest_of_the_published_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob.bin");
    let data: Vec<u8> = (0..300_000u32).map(|n| (n % 253) as u8).collect();
    let digest = atomic_write_with_digest(&path, AtomicWriteOptions::default(), |f| {
        // Written in pieces, straight to the file handle.
        for piece in data.chunks(10_000) {
            f.write_all(piece)?;
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), data);
    assert_eq!(digest, sha256_bytes(&data));
}

#[test]
fn commit_with_digest_covers_writes_through_either_path_and_a_failed_commit_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.bin");
    let mut writer = AtomicWriter::create(&path, AtomicOptions::default()).unwrap();
    writer.write_all(b"head-").unwrap();
    std::io::Write::write_all(writer.file_mut(), b"tail").unwrap();
    let digest = writer.commit_with_digest().unwrap();
    assert_eq!(digest, sha256_bytes(b"head-tail"));
    assert_eq!(std::fs::read(&path).unwrap(), b"head-tail");
}
