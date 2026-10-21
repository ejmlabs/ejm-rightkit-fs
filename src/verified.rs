//! Verified copy & publish, with `fs` plus `digest` (SHA-256) or `blake3`.
//!
//! [`copy_verified_with_digest`] copies a regular file to a temp file beside the destination, hashes the
//! bytes it read, optionally compares them with an expected digest, re-reads the written temp
//! file & compares again, then publishes it atomically. Pre-publication failures preserve
//! destination contents & remove partial temp files. A post-publication error reports that
//! replacement happened (see [`FsError`]).
//!
//! The source is read once through one descriptor and is refused if it is a symlink or not a
//! regular file, or if its length changed while it was copied. It is not locked: a writer that
//! keeps the length but changes bytes mid-copy is caught only when an expected digest was given.

use crate::atomic::{AtomicWriteOptions, AtomicWriter};
#[cfg(feature = "blake3")]
use crate::digest::Blake3Hasher;
#[cfg(feature = "digest")]
use crate::digest::Sha256Hasher;
use crate::error::{FsError, Result};
use std::cell::RefCell;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// What [`copy_verified`] published.
#[cfg(feature = "digest")]
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifiedCopy {
    /// Lowercase hex SHA-256 of the copied bytes (source and published copy agree).
    pub sha256: String,
    /// Number of bytes copied.
    pub bytes: u64,
}

/// Copy `src` to `dest` atomically and verify it. `expected_sha256` (hex, any case) is compared
/// with the source's digest when given: a mismatch is [`FsError::DigestMismatch`] and nothing is
/// published. `options` is the usual atomic-write policy (`private`, `durable`, replace mode).
#[cfg(feature = "digest")]
pub fn copy_verified(
    src: &Path,
    dest: &Path,
    options: AtomicWriteOptions,
    expected_sha256: Option<&str>,
) -> Result<VerifiedCopy> {
    copy_verified_with_progress(src, dest, options, expected_sha256, |_, _| {})
}

/// SHA-256 verified copy with byte progress, as documented by [`copy_verified_with_digest`].
#[cfg(feature = "digest")]
pub fn copy_verified_with_progress(
    src: &Path,
    dest: &Path,
    options: AtomicWriteOptions,
    expected_sha256: Option<&str>,
    progress: impl FnMut(u64, u64),
) -> Result<VerifiedCopy> {
    let copied = copy_verified_with_digest(
        src,
        dest,
        options,
        DigestAlgorithm::Sha256,
        expected_sha256,
        progress,
    )?;
    Ok(VerifiedCopy {
        sha256: copied.digest,
        bytes: copied.bytes,
    })
}

/// Algorithm used for both source & written-temp verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DigestAlgorithm {
    /// SHA-256, enabled by `digest`.
    #[cfg(feature = "digest")]
    Sha256,
    /// Unkeyed BLAKE3, enabled by `blake3`.
    #[cfg(feature = "blake3")]
    Blake3,
}

/// Contents published by [`copy_verified_with_digest`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifiedDigestCopy {
    /// Algorithm used for `digest`.
    pub algorithm: DigestAlgorithm,
    /// Lowercase hex digest of source bytes & verified written bytes.
    pub digest: String,
    /// Bytes copied.
    pub bytes: u64,
}

enum Hasher {
    #[cfg(feature = "digest")]
    Sha256(Sha256Hasher),
    #[cfg(feature = "blake3")]
    Blake3(Box<Blake3Hasher>),
}

impl DigestAlgorithm {
    fn hasher(self) -> Hasher {
        match self {
            #[cfg(feature = "digest")]
            Self::Sha256 => Hasher::Sha256(Sha256Hasher::new()),
            #[cfg(feature = "blake3")]
            Self::Blake3 => Hasher::Blake3(Box::new(Blake3Hasher::new())),
        }
    }

    fn file_with_len(self, path: &Path) -> std::io::Result<(String, u64)> {
        match self {
            #[cfg(feature = "digest")]
            Self::Sha256 => crate::digest::sha256_file_with_len(path),
            #[cfg(feature = "blake3")]
            Self::Blake3 => crate::digest::blake3_file_with_len(path),
        }
    }
}

impl Hasher {
    fn update(&mut self, bytes: &[u8]) {
        match self {
            #[cfg(feature = "digest")]
            Self::Sha256(h) => h.update(bytes),
            #[cfg(feature = "blake3")]
            Self::Blake3(h) => h.update(bytes),
        }
    }
    fn finalize(self) -> String {
        match self {
            #[cfg(feature = "digest")]
            Self::Sha256(h) => h.finalize(),
            #[cfg(feature = "blake3")]
            Self::Blake3(h) => h.finalize(),
        }
    }
}

/// Copy, hash, verify written bytes using `algorithm`, then publish atomically.
/// `expected_digest` accepts hex in either case. Mismatch returns [`FsError::DigestMismatch`]
/// before publication; dropping the writer removes its temporary file.
///
/// `progress(copied_bytes, initial_source_length)` runs synchronously once at zero,
/// then after each successful write of at most 64 KiB. An empty copy reports `(0, 0)`.
/// It covers copying only: reaching source length does not mean verification or
/// publication succeeded. No callbacks occur during temp verification or publication.
/// Length is a snapshot & may change during copying; source-length changes are refused.
/// A callback panic unwinds normally & drops the writer, removing its temp file.
///
/// Atomic options, symlink refusal & publication errors match [`AtomicWriter`]. Errors
/// before publication leave `dest` unchanged; directory-sync or post-publish cleanup
/// errors can report failure after replacement (see [`FsError`]).
pub fn copy_verified_with_digest(
    src: &Path,
    dest: &Path,
    options: AtomicWriteOptions,
    algorithm: DigestAlgorithm,
    expected_digest: Option<&str>,
    mut progress: impl FnMut(u64, u64),
) -> Result<VerifiedDigestCopy> {
    let meta = std::fs::symlink_metadata(src).map_err(|e| FsError::io(src, e))?;
    if meta.file_type().is_symlink() {
        return Err(FsError::Symlink(src.to_path_buf()));
    }
    if !meta.is_file() {
        return Err(FsError::NotAFile(src.to_path_buf()));
    }
    let mut input = File::open(src).map_err(|e| FsError::io(src, e))?;
    let mut writer = AtomicWriter::create_with(dest, options)?;
    let mut hasher = algorithm.hasher();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    let initial_length = input.metadata().map_err(|e| FsError::io(src, e))?.len();
    progress(0, initial_length);
    loop {
        let n = match input.read(&mut buffer) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result.map_err(|e| FsError::io(src, e))?,
        };
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        writer.write_all(&buffer[..n])?;
        total += n as u64;
        progress(total, initial_length);
    }
    let digest = hasher.finalize();
    let length_now = input.metadata().map_err(|e| FsError::io(src, e))?.len();
    if length_now != total || initial_length != total {
        return Err(FsError::Corrupt {
            path: src.to_path_buf(),
            reason: format!(
                "source changed length while it was copied ({initial_length} initially, {total} read, {length_now} now)"
            ),
        });
    }
    if let Some(expected) = expected_digest {
        if !expected.eq_ignore_ascii_case(&digest) {
            return Err(FsError::DigestMismatch {
                path: src.to_path_buf(),
                expected: expected.to_ascii_lowercase(),
                actual: digest,
            });
        }
    }
    // The check runs on the synced temp file, so it verifies what was written, not what was read.
    let written = RefCell::new(None::<String>);
    let result = writer.commit_checked(|temp| {
        let (actual, len) = algorithm.file_with_len(temp)?;
        if actual != digest || len != total {
            *written.borrow_mut() = Some(actual);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the written copy does not match the source",
            ));
        }
        Ok(())
    });
    match result {
        Ok(()) => Ok(VerifiedDigestCopy {
            algorithm,
            digest,
            bytes: total,
        }),
        Err(error) => match written.into_inner() {
            Some(actual) => Err(FsError::DigestMismatch {
                path: dest.to_path_buf(),
                expected: digest,
                actual,
            }),
            None => Err(error),
        },
    }
}
