//! Durable single-record append (M8-O3: ledgers, journals and source lists).
//!
//! [`append_record`] adds one record to the end of a file with one `write` on an `O_APPEND`
//! descriptor, then flushes. It is the append counterpart of [`crate::atomic_write`]; log files
//! with rotation belong to `rightkit-logs`, not here.
//!
//! What it guarantees, and what it does not:
//!
//! - One call writes its bytes with a single `write_all` to an append-mode handle. On a local
//!   filesystem concurrent appenders (threads or processes) therefore do not interleave inside a
//!   record that fits one `write`. This crate does not take a lock, and network filesystems
//!   give no such ordering ([`crate::is_network_filesystem`]).
//! - With `durable` the file is flushed with [`crate::durable_sync_with`] and the level reached
//!   is returned; a file this call created also has its directory flushed, so the name survives
//!   a power loss.
//! - A failed or interrupted append can leave a partial record at the end of the file (a full
//!   disk, a killed process). This crate does not truncate it back: with concurrent appenders
//!   the length it would restore is not knowable. Readers must tolerate a torn final record, for
//!   example by writing a trailing newline and ignoring an unterminated last line. Torn-tail
//!   repair is a domain decision and stays in the app.
//! - A symlink at `path` is refused ([`FsError::Symlink`]) and so is a name shaped like a temp
//!   name this crate generates ([`FsError::BadName`]).

use crate::atomic::{durable_sync_with, SyncLevel, SyncPolicy};
use crate::error::{FsError, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// How [`append_record`] creates and flushes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AppendOptions {
    /// A file this call creates is owner-only (0600 / owner DACL), as [`crate::create_private_new`].
    /// An existing file is appended to as it is; its permissions are never changed.
    pub private: bool,
    /// Flush the file after the write (and the directory when this call created the file).
    /// Default `true`.
    pub durable: bool,
}

impl Default for AppendOptions {
    fn default() -> Self {
        Self {
            private: false,
            durable: true,
        }
    }
}

impl AppendOptions {
    /// Durable, and owner-only when the file has to be created.
    pub fn private() -> Self {
        Self {
            private: true,
            ..Self::default()
        }
    }

    /// Set whether the append flushes (default `true`). The struct is `#[non_exhaustive]`, so
    /// callers outside this crate choose it here instead of with a struct expression.
    #[must_use]
    pub fn durable(mut self, durable: bool) -> Self {
        self.durable = durable;
        self
    }
}

/// Append `bytes` to `path`, creating the file when it is missing. Returns the [`SyncLevel`] the
/// flush reached, or `None` when `options.durable` is false. See the module docs for the
/// guarantees and the torn-tail limit.
pub fn append_record(
    path: &Path,
    bytes: &[u8],
    options: AppendOptions,
) -> Result<Option<SyncLevel>> {
    let name = path
        .file_name()
        .ok_or_else(|| FsError::BadName(path.display().to_string()))?;
    crate::atomic::reject_reserved_name(&name.to_string_lossy())?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(FsError::Symlink(path.to_path_buf()))
        }
        Ok(meta) if !meta.is_file() => return Err(FsError::NotAFile(path.to_path_buf())),
        _ => {}
    }
    let (mut file, created) = open_append(path, options)?;
    file.write_all(bytes).map_err(|e| FsError::io(path, e))?;
    if !options.durable {
        return Ok(None);
    }
    let level = durable_sync_with(&file, SyncPolicy::Fallback).map_err(|e| FsError::io(path, e))?;
    if created {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        crate::atomic::try_sync_dir(parent).map_err(|source| FsError::DirSyncFailed {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(Some(level))
}

/// Opens `path` for appending and returns the handle, for a caller that writes records of its own
/// (a chat transcript, a delivery journal) and does not want the one-call [`append_record`].
///
/// A file this call creates is owner-only (0600 / owner DACL), as [`crate::create_private_new`]; an
/// existing file keeps its permissions. The handle is append-mode (`O_APPEND`), so every write goes
/// to the current end. Nothing is flushed here: the caller syncs the handle when it needs the bytes
/// on stable storage ([`crate::durable_sync`]). A symlink at `path` is refused
/// ([`FsError::Symlink`]), a non-regular file is refused ([`FsError::NotAFile`]), and a name
/// shaped like a generated temp name is refused ([`FsError::BadName`]). The torn-tail caveat of
/// [`append_record`] applies to whatever the caller writes.
pub fn open_private_append(path: &Path) -> Result<std::fs::File> {
    let name = path
        .file_name()
        .ok_or_else(|| FsError::BadName(path.display().to_string()))?;
    crate::atomic::reject_reserved_name(&name.to_string_lossy())?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(FsError::Symlink(path.to_path_buf()))
        }
        Ok(meta) if !meta.is_file() => return Err(FsError::NotAFile(path.to_path_buf())),
        _ => {}
    }
    open_append(path, AppendOptions::private()).map(|(file, _)| file)
}

/// Opens `path` for appending; the flag says whether this call created it.
fn open_append(path: &Path, options: AppendOptions) -> Result<(std::fs::File, bool)> {
    if options.private {
        match crate::private::create_private_new(path) {
            // Created empty and owner-only; the descriptor is write-only, not append-mode, so
            // reopen it in append mode (nothing else has written to it yet that matters: any
            // other appender also opens in append mode).
            Ok(created) => {
                drop(created);
                let file = append_handle(path)?;
                return Ok((file, true));
            }
            Err(e) if e.kind() == Some(std::io::ErrorKind::AlreadyExists) => {}
            Err(e) => return Err(e),
        }
        return Ok((append_handle(path)?, false));
    }
    let existed = path.exists();
    let file = append_handle(path)?;
    Ok((file, !existed))
}

fn append_handle(path: &Path) -> Result<std::fs::File> {
    let mut open = OpenOptions::new();
    open.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    open.open(path).map_err(|e| FsError::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_private_append_creates_then_appends_to_the_same_file() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.log");
        {
            let mut first = open_private_append(&path).unwrap();
            first.write_all(b"one\n").unwrap();
        }
        {
            let mut second = open_private_append(&path).unwrap();
            second.write_all(b"two\n").unwrap();
        }
        let mut text = String::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "one\ntwo\n");
    }

    #[test]
    fn open_private_append_refuses_a_directory_and_a_temp_shaped_name() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open_private_append(dir.path()).is_err());
        let generated = dir.path().join(".x.rktmp-1-0-0");
        assert!(open_private_append(&generated).is_err());
    }
}
