//! A deterministic ZIP writer that stores entries uncompressed.
//!
//! [`StoredZipWriter`] writes the *stored* method only (method 0, no compression), which is what a
//! QA bundle, a test fixture or an evidence archive needs: the same entries in the same order give the
//! same bytes, on every platform and every run. Nothing varies: the timestamp of every entry is the
//! DOS epoch (1980-01-01 00:00:00), there are no extra fields and no comments, the host and the
//! attributes are fixed (Unix `0644` for a file, `0755` for a directory), and entries appear in the
//! order they were added. There is no compression and no ZIP64: an archive of more than 65 535
//! entries, or with any entry, local-header offset or central directory beyond 4 GiB - 1, is refused
//! with `InvalidInput` rather than written wrongly.
//!
//! Entry names are checked, since a name is the part of an archive that attacks the program that
//! unpacks it: a name must be non-empty, relative, use `/` only (no `\`, no drive prefix), have no
//! empty, `.` or `..` component, no NUL or control character, fit in 65 535 bytes, and be unique
//! (compared byte for byte). Names are written as UTF-8 and flagged as such (general-purpose bit 11).
//!
//! [`crc32`] is the standard IEEE CRC-32 (the one ZIP stores) computed inline; this crate adds no
//! dependency for it.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// The DOS time of every entry: 00:00:00.
const DOS_TIME: u16 = 0;
/// The DOS date of every entry: 1980-01-01 (year 0, month 1, day 1).
const DOS_DATE: u16 = (1 << 5) | 1;
/// General-purpose flag bit 11: the name is UTF-8.
const FLAG_UTF8: u16 = 0x0800;
/// "Version needed to extract": 2.0, the first that supports directories and stored entries alike.
const VERSION_NEEDED: u16 = 20;
/// "Version made by": host Unix (3), specification 2.0.
const VERSION_MADE_BY: u16 = (3 << 8) | 20;
const FILE_ATTRIBUTES: u32 = 0o100_644 << 16;
const DIR_ATTRIBUTES: u32 = (0o040_755 << 16) | 0x10;
const MAX_ENTRIES: usize = 0xFFFF;
const MAX_U32: u64 = 0xFFFF_FFFF;

const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_END: u32 = 0x0605_4b50;

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

/// The IEEE CRC-32 of `bytes` (polynomial `0xEDB88320`, initial value and final XOR `0xFFFFFFFF`), the
/// checksum a ZIP entry stores. `crc32(b"123456789")` is `0xCBF43926`.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc = CRC_TABLE[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// Check one entry name; see the module docs for the rules. `directory` names may end in one `/`.
fn check_name(name: &str, directory: bool) -> io::Result<()> {
    let body = if directory {
        name.strip_suffix('/').unwrap_or(name)
    } else {
        name
    };
    if body.is_empty() {
        return Err(invalid("a ZIP entry name is empty"));
    }
    if name.len() > 0xFFFF {
        return Err(invalid("a ZIP entry name is longer than 65535 bytes"));
    }
    if name.starts_with('/') || name.contains('\\') {
        return Err(invalid(format!(
            "ZIP entry name {name:?} is absolute or uses a backslash"
        )));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(invalid(format!(
            "ZIP entry name {name:?} has a control character"
        )));
    }
    let bytes = body.as_bytes();
    // `C:` or `C:x` as the first component is a drive prefix on Windows.
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(invalid(format!(
            "ZIP entry name {name:?} has a drive prefix"
        )));
    }
    for component in body.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(invalid(format!(
                "ZIP entry name {name:?} has an empty, `.` or `..` component"
            )));
        }
    }
    Ok(())
}

struct Entry {
    name: String,
    crc: u32,
    size: u32,
    offset: u32,
    directory: bool,
}

/// Writes a stored ZIP archive to `W`. Add entries, then [`finish`](Self::finish). Nothing is
/// flushed or synced: wrap a file in [`crate::AtomicWriter`] (or write the finished bytes with
/// [`crate::atomic_write`]) when the archive must appear whole.
pub struct StoredZipWriter<W: Write> {
    out: W,
    written: u64,
    entries: Vec<Entry>,
    seen: HashSet<String>,
}

impl<W: Write> StoredZipWriter<W> {
    /// A writer that appends to `out` from its current position (the archive starts there).
    pub fn new(out: W) -> Self {
        Self {
            out,
            written: 0,
            entries: Vec::new(),
            seen: HashSet::new(),
        }
    }

    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn register(&mut self, name: &str, directory: bool) -> io::Result<String> {
        check_name(name, directory)?;
        if self.entries.len() >= MAX_ENTRIES {
            return Err(invalid(
                "a ZIP archive without ZIP64 holds at most 65535 entries",
            ));
        }
        let stored = if directory && !name.ends_with('/') {
            format!("{name}/")
        } else {
            name.to_string()
        };
        if stored.len() > 0xFFFF {
            return Err(invalid("a ZIP entry name is longer than 65535 bytes"));
        }
        // A file `a` and a directory `a/` are different names but cannot both be unpacked.
        let twin = if directory {
            stored.trim_end_matches('/').to_string()
        } else {
            format!("{stored}/")
        };
        if self.seen.contains(&stored) || self.seen.contains(&twin) {
            return Err(invalid(format!("ZIP entry name {name:?} is already used")));
        }
        self.seen.insert(stored.clone());
        Ok(stored)
    }

    /// Add a file entry holding `data`.
    ///
    /// Errors (`InvalidInput`): a bad or duplicate name, `data` of 4 GiB or more, an archive past the
    /// ZIP limits, or a local-header offset past 4 GiB. A failed write is the writer's error.
    pub fn add_file(&mut self, name: &str, data: &[u8]) -> io::Result<()> {
        let size = u32::try_from(data.len())
            .map_err(|_| invalid("a ZIP entry without ZIP64 is under 4 GiB"))?;
        let stored = self.register(name, false)?;
        self.write_local(stored, data, size, false)
    }

    /// Add a directory entry (`name` with or without a trailing `/`; the stored name ends in `/`).
    pub fn add_dir(&mut self, name: &str) -> io::Result<()> {
        let stored = self.register(name, true)?;
        self.write_local(stored, &[], 0, true)
    }

    fn write_local(
        &mut self,
        name: String,
        data: &[u8],
        size: u32,
        directory: bool,
    ) -> io::Result<()> {
        let offset = u32::try_from(self.written)
            .map_err(|_| invalid("a ZIP entry starts past 4 GiB, which needs ZIP64"))?;
        let crc = crc32(data);
        let name_bytes = name.as_bytes();
        let mut header = Vec::with_capacity(30 + name_bytes.len());
        header.extend_from_slice(&SIG_LOCAL.to_le_bytes());
        header.extend_from_slice(&VERSION_NEEDED.to_le_bytes());
        header.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        header.extend_from_slice(&DOS_TIME.to_le_bytes());
        header.extend_from_slice(&DOS_DATE.to_le_bytes());
        header.extend_from_slice(&crc.to_le_bytes());
        header.extend_from_slice(&size.to_le_bytes()); // compressed size
        header.extend_from_slice(&size.to_le_bytes()); // uncompressed size
        header.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes()); // extra field length
        header.extend_from_slice(name_bytes);
        self.put(&header)?;
        self.put(data)?;
        self.entries.push(Entry {
            name,
            crc,
            size,
            offset,
            directory,
        });
        Ok(())
    }

    /// Write the central directory and end record and return the underlying writer. The archive is
    /// complete only after this succeeds.
    pub fn finish(mut self) -> io::Result<W> {
        let directory_offset = u32::try_from(self.written).map_err(|_| {
            invalid("the ZIP central directory starts past 4 GiB, which needs ZIP64")
        })?;
        let mut central = Vec::new();
        for entry in &self.entries {
            let name = entry.name.as_bytes();
            central.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
            central.extend_from_slice(&VERSION_MADE_BY.to_le_bytes());
            central.extend_from_slice(&VERSION_NEEDED.to_le_bytes());
            central.extend_from_slice(&FLAG_UTF8.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            central.extend_from_slice(&DOS_TIME.to_le_bytes());
            central.extend_from_slice(&DOS_DATE.to_le_bytes());
            central.extend_from_slice(&entry.crc.to_le_bytes());
            central.extend_from_slice(&entry.size.to_le_bytes());
            central.extend_from_slice(&entry.size.to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra field length
            central.extend_from_slice(&0u16.to_le_bytes()); // comment length
            central.extend_from_slice(&0u16.to_le_bytes()); // disk number start
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
            let attributes = if entry.directory {
                DIR_ATTRIBUTES
            } else {
                FILE_ATTRIBUTES
            };
            central.extend_from_slice(&attributes.to_le_bytes());
            central.extend_from_slice(&entry.offset.to_le_bytes());
            central.extend_from_slice(name);
        }
        let directory_size = u32::try_from(central.len())
            .ok()
            .filter(|size| u64::from(*size) + u64::from(directory_offset) <= MAX_U32)
            .ok_or_else(|| {
                invalid("the ZIP central directory ends past 4 GiB, which needs ZIP64")
            })?;
        let count = self.entries.len() as u16;
        let mut end = Vec::with_capacity(22);
        end.extend_from_slice(&SIG_END.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes()); // this disk
        end.extend_from_slice(&0u16.to_le_bytes()); // disk with the central directory
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&directory_size.to_le_bytes());
        end.extend_from_slice(&directory_offset.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes()); // comment length
        self.put(&central)?;
        self.put(&end)?;
        Ok(self.out)
    }
}

/// The bytes of a stored ZIP archive holding `entries` in order, each `(name, bytes)` a file; a name
/// ending in `/` with empty bytes is a directory entry. Same rules and limits as
/// [`StoredZipWriter`].
pub fn stored_zip_bytes(entries: &[(&str, &[u8])]) -> io::Result<Vec<u8>> {
    let mut writer = StoredZipWriter::new(Vec::new());
    for (name, data) in entries {
        if name.ends_with('/') {
            if !data.is_empty() {
                return Err(invalid(format!("directory entry {name:?} has content")));
            }
            writer.add_dir(name)?;
        } else {
            writer.add_file(name, data)?;
        }
    }
    writer.finish()
}

// Archives built from files on disk, byte-compatible with CutRight's former writer (rightkit#467).

/// "Version made by" of [`write_zip`]: host Unix (3), specification 3.0. This is the byte-exact
/// value CutRight's former `cutright-qa/src/zip.rs` wrote; [`StoredZipWriter`] keeps its own 2.0.
const FILE_ZIP_MADE_BY: u16 = 0x031E;

fn io_context(context: String, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

/// The CRC-32 (IEEE, as [`crc32`]) lookup table, for use with [`crc32_update`].
pub fn crc_table() -> [u32; 256] {
    CRC_TABLE
}

/// Extend CRC-32 with `bytes`; start at zero & pass each returned CRC to the next call.
pub fn crc32_update(table: &[u32; 256], crc: u32, bytes: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in bytes {
        c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

fn file_crc_size(path: &Path) -> io::Result<(u32, u64)> {
    let table = crc_table();
    let mut f =
        File::open(path).map_err(|e| io_context(format!("opening {}", path.display()), e))?;
    let mut crc = 0u32;
    let mut size = 0u64;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| io_context(format!("reading {}", path.display()), e))?;
        if n == 0 {
            break;
        }
        crc = crc32_update(&table, crc, &buf[..n]);
        size += n as u64;
    }
    Ok((crc, size))
}

/// Write `entries` (UTF-8 archive name, source file) to `dest`, in caller-supplied order.
/// Uses STORED entries, ZIP epoch (1980-01-01 00:00:00), Unix creator version 3.0,
/// fixed regular-file mode 0644, UTF-8 flags, & no extras/comments/data descriptors.
/// Identical ordered names & file bytes produce byte-identical archives across runs.
/// Source mtimes, permissions & host platform do not affect output.
///
/// ZIP64 is unsupported: fewer than 65535 entries, names at most 65535 UTF-8 bytes,
/// & sizes/offsets below 0xffffffff. Invalid limits return `InvalidInput`.
/// A source whose bytes change between the CRC pass and the copy pass (same size or not) is
/// rejected with `InvalidInput` rather than archived under the wrong CRC. Names are stored
/// verbatim, including separators & duplicates; callers choose extraction-safe names.
/// `dest` is created/truncated directly; errors can leave partial output. It must not
/// alias any source. Success flushes the buffer but does not fsync or publish atomically.
pub fn write_zip(dest: &Path, entries: &[(String, PathBuf)]) -> io::Result<()> {
    if entries.len() >= 0xFFFF {
        return Err(invalid("too many files for a zip"));
    }
    for (name, _) in entries {
        if name.len() > u16::MAX as usize {
            return Err(invalid("archive name exceeds 65535 UTF-8 bytes"));
        }
    }
    let io = |e| io_context(format!("writing {}", dest.display()), e);
    let mut out = BufWriter::new(File::create(dest).map_err(io)?);
    let mut offset: u64 = 0;
    let mut central: Vec<u8> = Vec::new();
    for (name, src) in entries {
        let (crc, size) = file_crc_size(src)?;
        if size >= 0xFFFF_FFFF || offset >= 0xFFFF_FFFF {
            return Err(invalid(format!(
                "{} is too large for a zip (ZIP64 is not written)",
                src.display()
            )));
        }
        // Reserve both records before narrowing lengths or offsets to ZIP32 fields.
        let name_bytes = name.as_bytes();
        checked_zip32(offset + 30 + name_bytes.len() as u64 + size)?;
        checked_zip32(central.len() as u64 + 46 + name_bytes.len() as u64)?;
        let mut local = Vec::with_capacity(30 + name_bytes.len());
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&VERSION_NEEDED.to_le_bytes());
        local.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // stored
        local.extend_from_slice(&DOS_TIME.to_le_bytes());
        local.extend_from_slice(&DOS_DATE.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&(size as u32).to_le_bytes());
        local.extend_from_slice(&(size as u32).to_le_bytes());
        local.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name_bytes);
        out.write_all(&local).map_err(io)?;
        let mut f =
            File::open(src).map_err(|e| io_context(format!("opening {}", src.display()), e))?;
        // Re-hash while copying: a same-size mutation since the CRC pass must be rejected, not
        // archived under the first pass's CRC (which `read_zip` would then refuse, and other
        // readers would misreport as mere corruption of good archived bytes).
        let (copied_crc, copied) = copy_counted_crc(&mut f, &mut out).map_err(io)?;
        if copied != size || copied_crc != crc {
            return Err(invalid(format!("{} changed while zipping", src.display())));
        }

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&FILE_ZIP_MADE_BY.to_le_bytes());
        central.extend_from_slice(&VERSION_NEEDED.to_le_bytes());
        central.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&DOS_TIME.to_le_bytes());
        central.extend_from_slice(&DOS_DATE.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(size as u32).to_le_bytes());
        central.extend_from_slice(&(size as u32).to_le_bytes());
        central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&FILE_ATTRIBUTES.to_le_bytes());
        central.extend_from_slice(&(offset as u32).to_le_bytes());
        central.extend_from_slice(name_bytes);
        offset += local.len() as u64 + size;
    }
    let cd_offset = offset;
    checked_zip32(cd_offset)?;
    checked_zip32(central.len() as u64)?;
    out.write_all(&central).map_err(io)?;
    let mut end = Vec::with_capacity(22);
    end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    end.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    end.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    end.extend_from_slice(&(central.len() as u32).to_le_bytes());
    end.extend_from_slice(&(cd_offset as u32).to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    out.write_all(&end).map_err(io)?;
    out.flush().map_err(io)?;
    Ok(())
}

/// Copy `src` to `out`, returning the CRC-32 and byte count of what was actually copied, so a
/// caller with an earlier fingerprint of the source can reject a source mutated mid-stream.
fn copy_counted_crc<R: Read, W: Write>(src: &mut R, out: &mut W) -> io::Result<(u32, u64)> {
    let table = crc_table();
    let mut crc = 0u32;
    let mut size = 0u64;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        crc = crc32_update(&table, crc, &buf[..n]);
        out.write_all(&buf[..n])?;
        size += n as u64;
    }
    Ok((crc, size))
}

fn checked_zip32(value: u64) -> io::Result<()> {
    if value >= u32::MAX as u64 {
        Err(invalid("archive exceeds ZIP32 limits"))
    } else {
        Ok(())
    }
}

/// Read archives produced by [`write_zip`], checking record bounds, sizes & every CRC.
/// Loads archive & entry contents into memory. This verifier supports only this writer's
/// STORED, UTF-8, no-extra/comment, single-disk format; other formats return `InvalidData`.
/// Malformed or truncated archives return errors without indexing outside input.
pub fn read_zip(path: &Path) -> io::Result<Vec<(String, Vec<u8>)>> {
    let data = std::fs::read(path)?;
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "invalid STORED zip");
    let u16at = |o: usize| -> io::Result<usize> {
        let bytes: [u8; 2] = data
            .get(o..o.checked_add(2).ok_or_else(bad)?)
            .ok_or_else(bad)?
            .try_into()
            .map_err(|_| bad())?;
        Ok(u16::from_le_bytes(bytes) as usize)
    };
    let u32at = |o: usize| -> io::Result<u32> {
        let bytes: [u8; 4] = data
            .get(o..o.checked_add(4).ok_or_else(bad)?)
            .ok_or_else(bad)?
            .try_into()
            .map_err(|_| bad())?;
        Ok(u32::from_le_bytes(bytes))
    };
    let eocd = data.len().checked_sub(22).ok_or_else(bad)?;
    if u32at(eocd)? != 0x0605_4b50
        || u16at(eocd + 4)? != 0
        || u16at(eocd + 6)? != 0
        || u16at(eocd + 20)? != 0
    {
        return Err(bad());
    }
    let count = u16at(eocd + 10)?;
    if count >= 0xffff || u16at(eocd + 8)? != count {
        return Err(bad());
    }
    let central_size = u32at(eocd + 12)? as usize;
    let central_start = u32at(eocd + 16)? as usize;
    if central_start.checked_add(central_size) != Some(eocd) {
        return Err(bad());
    }
    let table = crc_table();
    let mut p = central_start;
    let mut local_end = 0;
    let mut out = Vec::new();
    for _ in 0..count {
        let header_end = p.checked_add(46).filter(|&n| n <= eocd).ok_or_else(bad)?;
        if u32at(p)? != 0x0201_4b50
            || u16at(p + 8)? != 0x0800
            || u16at(p + 10)? != 0
            || u16at(p + 30)? != 0
            || u16at(p + 32)? != 0
            || u16at(p + 34)? != 0
        {
            return Err(bad());
        }
        let crc = u32at(p + 16)?;
        let size = u32at(p + 24)? as usize;
        if u32at(p + 20)? as usize != size {
            return Err(bad());
        }
        let name_len = u16at(p + 28)?;
        let next = header_end
            .checked_add(name_len)
            .filter(|&n| n <= eocd)
            .ok_or_else(bad)?;
        let name_bytes = &data[header_end..next];
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| bad())?
            .to_owned();
        let local = u32at(p + 42)? as usize;
        let start = local
            .checked_add(30)
            .and_then(|n| n.checked_add(name_len))
            .filter(|&n| n <= central_start)
            .ok_or_else(bad)?;
        let end = start
            .checked_add(size)
            .filter(|&n| n <= central_start)
            .ok_or_else(bad)?;
        if local != local_end
            || u32at(local)? != 0x0403_4b50
            || u16at(local + 6)? != 0x0800
            || u16at(local + 8)? != 0
            || u32at(local + 14)? != crc
            || u32at(local + 18)? as usize != size
            || u32at(local + 22)? as usize != size
            || u16at(local + 26)? != name_len
            || u16at(local + 28)? != 0
            || &data[local + 30..start] != name_bytes
        {
            return Err(bad());
        }
        let bytes = &data[start..end];
        if crc32_update(&table, 0, bytes) != crc {
            return Err(bad());
        }
        out.push((name, bytes.to_vec()));
        local_end = end;
        p = next;
    }
    if p != eocd || local_end != central_start {
        return Err(bad());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le16(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    fn le32(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    #[test]
    fn crc32_matches_the_standard_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"a"), 0xE8B7_BE43);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
        assert_eq!(crc32(&[0u8; 32]), 0x190A_55AD);
    }

    #[test]
    fn two_writes_of_the_same_entries_are_byte_identical() {
        let entries: [(&str, &[u8]); 3] = [
            ("a.txt", b"alpha"),
            ("dir/", b""),
            ("dir/b.bin", &[0, 1, 2, 3, 255]),
        ];
        let first = stored_zip_bytes(&entries).unwrap();
        let second = stored_zip_bytes(&entries).unwrap();
        assert_eq!(first, second);
        // Nothing wall-clock or host dependent is in the bytes: pin the whole archive's length and
        // one stable field of each header.
        assert_eq!(le32(&first, 0), SIG_LOCAL);
        assert_eq!(le16(&first, 10), DOS_TIME);
        assert_eq!(le16(&first, 12), DOS_DATE);
    }

    #[test]
    fn the_layout_parses_back_to_the_entries() {
        let bytes =
            stored_zip_bytes(&[("a.txt", b"alpha"), ("sub/", b""), ("sub/b", b"beta!")]).unwrap();
        // End of central directory is the last 22 bytes.
        let end = bytes.len() - 22;
        assert_eq!(le32(&bytes, end), SIG_END);
        assert_eq!(le16(&bytes, end + 8), 3, "entries on this disk");
        assert_eq!(le16(&bytes, end + 10), 3, "entries in total");
        let directory_size = le32(&bytes, end + 12) as usize;
        let directory_offset = le32(&bytes, end + 16) as usize;
        assert_eq!(directory_offset + directory_size, end);
        assert_eq!(le16(&bytes, end + 20), 0, "no comment");

        let mut at = directory_offset;
        let mut seen = Vec::new();
        for _ in 0..3 {
            assert_eq!(le32(&bytes, at), SIG_CENTRAL);
            assert_eq!(le16(&bytes, at + 8), FLAG_UTF8);
            assert_eq!(le16(&bytes, at + 10), 0, "stored");
            let crc = le32(&bytes, at + 16);
            let size = le32(&bytes, at + 20) as usize;
            assert_eq!(le32(&bytes, at + 24) as usize, size);
            let name_len = le16(&bytes, at + 28) as usize;
            assert_eq!(le16(&bytes, at + 30), 0, "no extra field");
            let offset = le32(&bytes, at + 42) as usize;
            let name = std::str::from_utf8(&bytes[at + 46..at + 46 + name_len]).unwrap();
            // The local header at that offset names the same entry, then comes the data.
            assert_eq!(le32(&bytes, offset), SIG_LOCAL);
            assert_eq!(le32(&bytes, offset + 14), crc);
            assert_eq!(le16(&bytes, offset + 26) as usize, name_len);
            let data_at = offset + 30 + name_len;
            let data = &bytes[data_at..data_at + size];
            assert_eq!(crc32(data), crc);
            seen.push((name.to_string(), data.to_vec()));
            at += 46 + name_len;
        }
        assert_eq!(at, end);
        assert_eq!(
            seen,
            [
                ("a.txt".to_string(), b"alpha".to_vec()),
                ("sub/".to_string(), Vec::new()),
                ("sub/b".to_string(), b"beta!".to_vec()),
            ]
        );
    }

    #[test]
    fn an_empty_archive_is_just_the_end_record() {
        let bytes = stored_zip_bytes(&[]).unwrap();
        assert_eq!(bytes.len(), 22);
        assert_eq!(le32(&bytes, 0), SIG_END);
        assert_eq!(le16(&bytes, 10), 0);
    }

    #[test]
    fn directories_get_a_trailing_slash_and_directory_attributes() {
        let mut writer = StoredZipWriter::new(Vec::new());
        writer.add_dir("d").unwrap();
        let bytes = writer.finish().unwrap();
        let end = bytes.len() - 22;
        let at = le32(&bytes, end + 16) as usize;
        let name_len = le16(&bytes, at + 28) as usize;
        assert_eq!(&bytes[at + 46..at + 46 + name_len], b"d/");
        assert_eq!(le32(&bytes, at + 38), DIR_ATTRIBUTES);
    }

    #[test]
    fn unsafe_names_are_refused() {
        let mut writer = StoredZipWriter::new(Vec::new());
        for bad in [
            "",
            "/",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a/./b",
            "a//b",
            "a\\b",
            "C:\\x",
            "C:x",
            "c:/x",
            "a\0b",
            "a\nb",
            "trailing//",
        ] {
            let err = writer.add_file(bad, b"x").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
        let long = "n".repeat(0x10000);
        assert!(writer.add_file(&long, b"x").is_err());
        // Nothing was written by the refusals.
        assert_eq!(writer.finish().unwrap().len(), 22);
    }

    #[test]
    fn duplicate_names_and_a_file_directory_clash_are_refused() {
        let mut writer = StoredZipWriter::new(Vec::new());
        writer.add_file("a", b"1").unwrap();
        assert!(writer.add_file("a", b"2").is_err());
        assert!(
            writer.add_dir("a").is_err(),
            "a file `a` and a directory `a/`"
        );
        writer.add_dir("d").unwrap();
        assert!(writer.add_dir("d/").is_err());
        assert!(writer.add_file("d", b"x").is_err());
        // Names that merely share a prefix are fine, and case differences are different bytes.
        writer.add_file("ab", b"3").unwrap();
        writer.add_file("A", b"4").unwrap();
    }

    #[test]
    fn utf8_names_are_stored_as_utf8() {
        let bytes = stored_zip_bytes(&[("r\u{e9}sum\u{e9}.txt", b"x")]).unwrap();
        let name = "r\u{e9}sum\u{e9}.txt".as_bytes();
        assert_eq!(le16(&bytes, 6), FLAG_UTF8);
        assert_eq!(&bytes[30..30 + name.len()], name);
    }

    #[test]
    fn a_directory_entry_with_content_is_refused_by_the_helper() {
        assert!(stored_zip_bytes(&[("d/", b"x")]).is_err());
    }

    #[test]
    fn a_failing_sink_surfaces_its_error() {
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::StorageFull))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = StoredZipWriter::new(Full);
        assert_eq!(
            writer.add_file("a", b"x").unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
    }
}

#[cfg(test)]
mod file_zip_tests {
    use super::*;

    #[test]
    fn crc_table_matches_known_value() {
        let t = crc_table();
        assert_eq!(crc32_update(&t, 0, b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn the_copy_pass_rehash_detects_a_same_size_mutation() {
        // What write_zip compares: the fingerprint of the bytes it actually streams, not just
        // their count. A same-size edit after the first pass changes the CRC and is rejected.
        let before: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut after = before.clone();
        after[50_000] ^= 0xFF;
        let mut sink = Vec::new();
        let (crc_before, size) = copy_counted_crc(&mut &before[..], &mut sink).unwrap();
        assert_eq!(sink, before);
        assert_eq!(size, before.len() as u64);
        assert_eq!(crc_before, crc32(&before));
        let (crc_after, same_size) = copy_counted_crc(&mut &after[..], &mut Vec::new()).unwrap();
        assert_eq!(same_size, size);
        assert_ne!(crc_after, crc_before);
    }

    #[test]
    fn round_trips_and_is_deterministic() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), b"alpha").unwrap();
        std::fs::write(d.path().join("b.bin"), vec![7u8; 100_000]).unwrap();
        let entries = vec![
            ("pkg/a.txt".to_string(), d.path().join("a.txt")),
            ("pkg/b.bin".to_string(), d.path().join("b.bin")),
        ];
        write_zip(&d.path().join("1.zip"), &entries).unwrap();
        write_zip(&d.path().join("2.zip"), &entries).unwrap();
        assert_eq!(
            std::fs::read(d.path().join("1.zip")).unwrap(),
            std::fs::read(d.path().join("2.zip")).unwrap()
        );
        let back = read_zip(&d.path().join("1.zip")).unwrap();
        assert_eq!(back[0], ("pkg/a.txt".to_string(), b"alpha".to_vec()));
        assert_eq!(back[1].1.len(), 100_000);
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;
    #[test]
    fn zip32_sentinel_is_refused() {
        assert!(checked_zip32(u32::MAX as u64 - 1).is_ok());
        assert_eq!(
            checked_zip32(u32::MAX as u64).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    #[test]
    fn crc_can_be_extended_across_chunks() {
        let t = crc_table();
        assert_eq!(
            crc32_update(&t, crc32_update(&t, 0, b"1234"), b"56789"),
            0xcbf43926
        );
    }
}
