//! SHA-256 (`digest` feature), BLAKE3 (`blake3` feature), & lowercase hex.
//!
//! Consolidates the file-digest and hex helpers that Right Suite crates each copied. File
//! digests stream in 64 KiB chunks, so large artifacts never load into memory.

#[cfg(feature = "digest")]
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const CHUNK: usize = 64 * 1024;

/// Lowercase hex of `bytes`: two characters per byte, `0-9a-f` only.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// True when every byte of `text` is an ASCII hex digit (`0-9`, `a-f`, `A-F`). The empty string is
/// hex (zero bytes). Pure, no allocation. Callers that name a file after the text check the length
/// too, so a hostile id cannot name a path.
pub fn is_hex(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Decode hex text (either case) into bytes. `None` when the length is odd or a character is not
/// an ASCII hex digit. The inverse of [`hex`] for lowercase input.
pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if !is_hex(text) || !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}

/// Incremental SHA-256 over this crate's own `sha2`, so a consumer that hashes while it writes needs no
/// second `sha2` major. Feed it with [`Sha256Hasher::update`] (or as an [`io::Write`]), then take the
/// lowercase hex with [`Sha256Hasher::finalize`]. The result equals [`sha256_bytes`] of the
/// concatenated input.
#[cfg(feature = "digest")]
#[derive(Clone, Default)]
pub struct Sha256Hasher(Sha256);

#[cfg(feature = "digest")]
impl Sha256Hasher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hash more input.
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// The lowercase hex digest of everything fed so far.
    pub fn finalize(self) -> String {
        hex(self.0.finalize().as_slice())
    }

    /// The raw 32-byte digest.
    pub fn finalize_bytes(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

#[cfg(feature = "digest")]
impl io::Write for Sha256Hasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Lowercase hex SHA-256 of `bytes`.
#[cfg(feature = "digest")]
pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes).as_slice())
}

/// Lowercase hex SHA-256 of everything `reader` yields, read to end-of-file.
#[cfg(feature = "digest")]
pub fn sha256_reader<R: Read>(reader: &mut R) -> io::Result<String> {
    sha256_reader_counted(reader).map(|(digest, _)| digest)
}

/// Lowercase hex SHA-256 of the file at `path`, streamed.
#[cfg(feature = "digest")]
pub fn sha256_file(path: &Path) -> io::Result<String> {
    sha256_file_with_len(path).map(|(digest, _)| digest)
}

/// As [`sha256_file`], also returning the number of bytes that were hashed.
#[cfg(feature = "digest")]
pub fn sha256_file_with_len(path: &Path) -> io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    sha256_reader_counted(&mut file)
}

#[cfg(feature = "digest")]
fn sha256_reader_counted<R: Read>(reader: &mut R) -> io::Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        total += n as u64;
    }
    Ok((hex(hasher.finalize().as_slice()), total))
}

#[cfg(all(test, feature = "digest"))]
mod tests {
    use super::*;

    // FIPS 180-2 test vector for "abc".
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn hex_is_lowercase_two_chars_per_byte() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn unhex_inverts_hex_and_accepts_either_case() {
        let bytes = [0x00u8, 0x0f, 0xa0, 0xff];
        assert_eq!(unhex(&hex(&bytes)), Some(bytes.to_vec()));
        assert_eq!(unhex("0FA0Ff"), Some(vec![0x0f, 0xa0, 0xff]));
        assert_eq!(unhex(""), Some(Vec::new()));
    }

    #[test]
    fn unhex_and_is_hex_refuse_odd_length_and_non_hex() {
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex("0g"), None);
        assert_eq!(unhex("é0"), None);
        assert!(is_hex("0123456789abcdefABCDEF"));
        assert!(!is_hex("0x12"));
        assert!(!is_hex("../x"));
    }

    #[test]
    fn bytes_digest_matches_known_vector() {
        assert_eq!(sha256_bytes(b"abc"), ABC);
    }

    #[test]
    fn streamed_file_digest_matches_bytes_digest_across_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        // Larger than one chunk so the loop runs more than once.
        let body: Vec<u8> = (0..(CHUNK * 2 + 17)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &body).unwrap();
        let (digest, len) = sha256_file_with_len(&path).unwrap();
        assert_eq!(digest, sha256_bytes(&body));
        assert_eq!(len, body.len() as u64);
        assert_eq!(sha256_file(&path).unwrap(), digest);
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = sha256_file(&dir.path().join("absent")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}

/// Incremental BLAKE3; results use lowercase hex unless raw bytes are requested.
#[cfg(feature = "blake3")]
#[derive(Clone, Default)]
pub struct Blake3Hasher(blake3::Hasher);

#[cfg(feature = "blake3")]
impl Blake3Hasher {
    /// Start an unkeyed BLAKE3 digest.
    pub fn new() -> Self {
        Self::default()
    }
    /// Hash more input.
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
    /// Lowercase hex of all input.
    pub fn finalize(self) -> String {
        self.0.finalize().to_hex().to_string()
    }
    /// Raw 32-byte digest of all input.
    pub fn finalize_bytes(self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}

#[cfg(feature = "blake3")]
impl io::Write for Blake3Hasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Lowercase hex BLAKE3 of `bytes`.
#[cfg(feature = "blake3")]
pub fn blake3_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Lowercase hex BLAKE3 of everything `reader` yields, streamed in 64 KiB chunks.
#[cfg(feature = "blake3")]
pub fn blake3_reader<R: Read>(reader: &mut R) -> io::Result<String> {
    blake3_reader_counted(reader, &mut |_| {}).map(|(digest, _)| digest)
}

/// Lowercase hex BLAKE3 of file contents, streamed.
#[cfg(feature = "blake3")]
pub fn blake3_file(path: &Path) -> io::Result<String> {
    blake3_file_with_len(path).map(|(digest, _)| digest)
}

/// As [`blake3_file`], also returning bytes hashed.
#[cfg(feature = "blake3")]
pub fn blake3_file_with_len(path: &Path) -> io::Result<(String, u64)> {
    blake3_file_with_progress(path, |_, _| {})
}

/// Hash file contents & report `(bytes_hashed, initial_file_length)` synchronously.
/// Called once with zero bytes, then after each 64 KiB read (or final shorter read).
/// An empty file reports `(0, 0)` once. Length is a snapshot: a concurrent writer may
/// change it; this function hashes until EOF & returns bytes actually read.
/// No callback occurs if opening or reading metadata fails. Read errors propagate.
#[cfg(feature = "blake3")]
pub fn blake3_file_with_progress(
    path: &Path,
    mut progress: impl FnMut(u64, u64),
) -> io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    progress(0, length);
    blake3_reader_counted(&mut file, &mut |bytes| progress(bytes, length))
}

#[cfg(feature = "blake3")]
fn blake3_reader_counted<R: Read>(
    reader: &mut R,
    progress: &mut dyn FnMut(u64),
) -> io::Result<(String, u64)> {
    let mut hasher = Blake3Hasher::new();
    let mut buffer = vec![0; CHUNK];
    let mut total = 0;
    loop {
        let n = match reader.read(&mut buffer) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        total += n as u64;
        progress(total);
    }
    Ok((hasher.finalize(), total))
}

#[cfg(all(test, feature = "blake3"))]
mod blake3_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn published_vectors_and_incremental_write() {
        assert_eq!(
            blake3_bytes(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        let abc = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";
        assert_eq!(blake3_bytes(b"abc"), abc);
        let mut hasher = Blake3Hasher::new();
        hasher.update(b"a");
        hasher.write_all(b"bc").unwrap();
        hasher.flush().unwrap();
        assert_eq!(hex(&hasher.clone().finalize_bytes()), abc);
        assert_eq!(hasher.finalize(), abc);
        assert_eq!(blake3_reader(&mut &b"abc"[..]).unwrap(), abc);
    }

    #[test]
    fn reader_propagates_failure_and_retries_interrupted_reads() {
        struct Interrupted(bool);
        impl Read for Interrupted {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                if !self.0 {
                    self.0 = true;
                    Err(io::ErrorKind::Interrupted.into())
                } else {
                    Ok(0)
                }
            }
        }
        assert_eq!(
            blake3_reader(&mut Interrupted(false)).unwrap(),
            blake3_bytes(b"")
        );
        struct Failed;
        impl Read for Failed {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::Other.into())
            }
        }
        assert_eq!(
            blake3_reader(&mut Failed).unwrap_err().kind(),
            io::ErrorKind::Other
        );
    }
}
