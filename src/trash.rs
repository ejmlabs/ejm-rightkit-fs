//! Move to the trash, find it again by identity, put it back (macOS and Windows).
//!
//! - [`move_to_trash`]: send a file, directory or symlink to the Trash (macOS) or Recycle Bin
//!   (Windows) and return a [`TrashIdentity`], a small value an app can persist.
//! - [`lookup`] and [`list`]: find what is in the trash, by identity or all of it.
//! - [`restore`]: move an item back to the path it came from, never over something that is there.
//!
//! The identity is the item's [`FileKey`] (device and inode, or volume serial and file index, which a
//! move within one volume preserves), its original absolute path, and the time it was trashed. A
//! lookup requires all three to agree (the trashed time within a window), because a bare inode number
//! can be reused by an unrelated file after the original is gone, and a bare path can be reused by a
//! new file.
//!
//! # macOS
//!
//! `FSMoveObjectToTrashSync` (CoreServices; the call Finder's "Move to Trash" is built on, without
//! Finder or a UI), hand-declared like the crate's other system calls. It picks the right trash for the
//! item's volume (`~/.Trash`, or `/Volumes/<name>/.Trashes/<uid>`) and renames on collision. The trashed
//! path comes back from the same API. A symlink is trashed as the link, not its target. Finder's
//! "Put Back" records live in `.DS_Store`, which this module neither reads nor writes: an item this
//! module trashes is restored by [`restore`], and Finder may or may not offer "Put Back" for it.
//! [`list`] covers `~/.Trash` and every mounted volume's `.Trashes/<uid>`; it cannot know an entry's
//! original path (`original` is `None`).
//!
//! # Windows
//!
//! Moving to the Recycle Bin is **refused** ([`FsError::Unavailable`]), before anything is touched.
//! The only call this module could reach without a COM binding is `SHFileOperationW`, and with
//! `FOF_NOCONFIRMATION` (needed for a call with no UI) it deletes an item permanently, with no way to
//! undo it, when the item is too large for the bin, the bin is disabled for the volume
//! (`NukeOnDelete`), or the shell otherwise cannot recycle it; without that flag it blocks on a
//! dialog. A trash function that can destroy its input is worse than none, so [`move_to_trash`] does
//! not delete on Windows until a recycle-only operation (`IFileOperation` with
//! `FOFX_RECYCLEONDELETE`, which fails instead of deleting) is available to this crate.
//!
//! [`lookup`], [`list`] and [`restore`] still work for items already in the bin: the shell keeps each
//! item as a pair in `<drive>:\$Recycle.Bin\<user SID>\`: `$I<id>` (a small record: original path,
//! size, deletion time) and `$R<id>` (the item itself). They read those records (format versions 1
//! and 2) and move `$R<id>` back; they never write a record. A UNC path, a network or removable
//! drive and a path that is not drive-lettered are refused with [`FsError::Unavailable`].
//!
//! Everything else (Linux, other targets) returns [`FsError::Unavailable`].
//!
//! The Swift counterpart ("RightKitSwift") is a separate repository and is not part of this crate.

use crate::error::{FsError, Result};
use crate::file_key::FileKey;
use std::path::{Path, PathBuf};

/// How long before the recorded trashing time an entry's own timestamp may still count as the same
/// trashing (clock steps, coarse timestamps). 60 s; an unrelated entry that reused an inode is older
/// than that unless it was trashed in the same minute.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const WINDOW_MS: u64 = 60_000;

/// The durable handle of a trashed item.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "json", derive(serde::Serialize, serde::Deserialize))]
pub struct TrashIdentity {
    /// The absolute path the item had before it was trashed (and where [`restore`] puts it back).
    pub original: PathBuf,
    /// The item's filesystem identity (the symlink's own for a symlink).
    pub key: FileKey,
    /// When it was trashed, milliseconds since the Unix epoch.
    pub trashed_unix_ms: u64,
    /// Where the trash put it, as a hint for a fast lookup; it can be stale (the trash was emptied or
    /// the entry renamed), so [`lookup`] checks it and falls back to scanning.
    pub trashed_as: Option<PathBuf>,
}

/// One item found in the trash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashEntry {
    /// Where the item is now (inside the trash).
    pub path: PathBuf,
    /// Its filesystem identity.
    pub key: FileKey,
    /// The path it came from, when the platform records it (Windows). `None` on macOS.
    pub original: Option<PathBuf>,
    /// When it was trashed: the Recycle Bin record's time (Windows) or the entry's change time
    /// (macOS), milliseconds since the Unix epoch.
    pub trashed_unix_ms: u64,
}

#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
fn unavailable() -> FsError {
    FsError::Unavailable("trash is available on macOS and Windows only")
}

/// Send `path` to the trash and return its identity. `path` must exist (a symlink is trashed itself,
/// never its target), be absolute or is made absolute against the current directory, and must not
/// hold a NUL byte. The item leaves `path`; on error it is where it was. On Windows this always
/// returns [`FsError::Unavailable`] without touching the item (see the module docs).
pub fn move_to_trash(path: &Path) -> Result<TrashIdentity> {
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(FsError::BadName(path.display().to_string()));
    }
    let original = std::path::absolute(path).map_err(|e| FsError::io(path, e))?;
    let key = sys::key_nofollow(&original)?;
    let before_ms = now_ms();
    let moved = sys::move_item(&original, key)?;
    Ok(TrashIdentity {
        original,
        key,
        trashed_unix_ms: moved.trashed_unix_ms.unwrap_or(before_ms),
        trashed_as: moved.trashed_as,
    })
}

/// Every item in the trash this module can see (see the platform sections of the module docs). Order
/// is unspecified. A trash that does not exist is an empty list, not an error.
pub fn list() -> Result<Vec<TrashEntry>> {
    sys::list()
}

/// The trashed item `identity` names, or `None` when it is no longer there (restored, emptied, or
/// never matched). Requires the key and the trashing time to agree (and on Windows the original
/// path), see the module docs.
pub fn lookup(identity: &TrashIdentity) -> Result<Option<TrashEntry>> {
    sys::lookup(identity)
}

/// Put the item `identity` names back at `identity.original` and return that path.
///
/// The item is found with [`lookup`] ([`FsError::Io`] with `NotFound` when it is not in the trash) and
/// moved with an exclusive rename: if something exists at the original path the result is
/// [`FsError::AlreadyExists`] and the item stays in the trash, so nothing is ever overwritten. The
/// original's parent directory must exist (it is not recreated). Windows then removes the Recycle Bin
/// record of the item on a best-effort basis (a leftover record without its data is ignored by
/// Explorer).
pub fn restore(identity: &TrashIdentity) -> Result<PathBuf> {
    let entry = lookup(identity)?.ok_or_else(|| {
        FsError::io(
            &identity.original,
            std::io::Error::new(std::io::ErrorKind::NotFound, "the item is not in the trash"),
        )
    })?;
    sys::restore(&entry, identity)?;
    Ok(identity.original.clone())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What a platform's move reports: where the item went and, when the platform records it, when.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Moved {
    trashed_as: Option<PathBuf>,
    trashed_unix_ms: Option<u64>,
}

/// Whether an entry's own timestamp is consistent with the trashing `identity_ms` it should belong
/// to: not older than the window before it. (An entry can be touched after trashing, so there is no
/// upper bound.)
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn entry_time_matches(entry_ms: u64, identity_ms: u64) -> bool {
    entry_ms.saturating_add(WINDOW_MS) >= identity_ms
}

/// FILETIME (100 ns ticks since 1601-01-01) to milliseconds since the Unix epoch, saturating at 0.
#[cfg_attr(not(windows), allow(dead_code))]
fn filetime_to_unix_ms(filetime: u64) -> u64 {
    const EPOCH_DIFFERENCE_MS: u64 = 11_644_473_600_000;
    (filetime / 10_000).saturating_sub(EPOCH_DIFFERENCE_MS)
}

/// One Recycle Bin `$I` record.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
struct RecycleInfo {
    #[allow(dead_code)]
    size: u64,
    deleted_unix_ms: u64,
    original: String,
}

/// Parse a Recycle Bin `$I` record. Version 1 (Vista to 8.1) is 544 bytes with a fixed 520-byte path
/// field; version 2 (Windows 10 and later) stores the path length in characters (with its NUL) and
/// then the path. Every length is bounds-checked; anything malformed is `None`.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_recycle_info(bytes: &[u8]) -> Option<RecycleInfo> {
    let u64_at = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
    };
    let version = u64_at(0)?;
    let size = u64_at(8)?;
    let deleted = u64_at(16)?;
    let path_bytes: &[u8] = match version {
        1 => {
            let rest = bytes.get(24..)?;
            &rest[..rest.len().min(520)]
        }
        2 => {
            let chars = u32::from_le_bytes(bytes.get(24..28)?.try_into().ok()?) as usize;
            if chars == 0 || chars > 32_768 + 1 {
                return None;
            }
            bytes.get(28..28usize.checked_add(chars.checked_mul(2)?)?)?
        }
        _ => return None,
    };
    let units: Vec<u16> = path_bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect();
    if units.is_empty() {
        return None;
    }
    Some(RecycleInfo {
        size,
        deleted_unix_ms: filetime_to_unix_ms(deleted),
        original: String::from_utf16(&units).ok()?,
    })
}

#[cfg(target_os = "macos")]
mod sys {
    use super::{entry_time_matches, FileKey, FsError, Moved, Result, TrashEntry, TrashIdentity};
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    /// `FSRef`, an opaque 80-byte record.
    #[repr(C)]
    struct FsRef {
        #[allow(dead_code)]
        hidden: [u8; 80],
    }

    /// `kFSPathMakeRefDoNotFollowLeafSymlink`: a symlink is trashed as the link.
    const DO_NOT_FOLLOW_LEAF_SYMLINK: u32 = 0x1;
    /// `kFSFileOperationDefaultOptions`.
    const DEFAULT_OPTIONS: u32 = 0;
    const NO_ERR: i32 = 0;

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn FSPathMakeRefWithOptions(
            path: *const u8,
            options: u32,
            fs_ref: *mut FsRef,
            is_directory: *mut u8,
        ) -> i32;
        fn FSMoveObjectToTrashSync(source: *const FsRef, target: *mut FsRef, options: u32) -> i32;
        fn FSRefMakePath(fs_ref: *const FsRef, path: *mut u8, max_size: u32) -> i32;
    }

    pub fn key_nofollow(path: &Path) -> Result<FileKey> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| FsError::io(path, e))?;
        Ok(FileKey {
            volume: meta.dev(),
            index: meta.ino(),
        })
    }

    fn os_status(path: &Path, call: &str, status: i32) -> FsError {
        FsError::io(
            path,
            std::io::Error::other(format!("{call} failed with OSStatus {status}")),
        )
    }

    pub fn move_item(original: &Path, _key: FileKey) -> Result<Moved> {
        let mut c_path = original.as_os_str().as_bytes().to_vec();
        c_path.push(0);
        let mut source = FsRef { hidden: [0; 80] };
        let mut target = FsRef { hidden: [0; 80] };
        // SAFETY: `c_path` is NUL-terminated and outlives the call; `source` is an out-parameter of
        // the 80-byte FSRef size; the directory flag out-parameter is optional (null).
        let status = unsafe {
            FSPathMakeRefWithOptions(
                c_path.as_ptr(),
                DO_NOT_FOLLOW_LEAF_SYMLINK,
                &mut source,
                std::ptr::null_mut(),
            )
        };
        if status != NO_ERR {
            return Err(os_status(original, "FSPathMakeRefWithOptions", status));
        }
        // SAFETY: `source` was filled by the call above; `target` is a valid out-parameter.
        let status = unsafe { FSMoveObjectToTrashSync(&source, &mut target, DEFAULT_OPTIONS) };
        if status != NO_ERR {
            return Err(os_status(original, "FSMoveObjectToTrashSync", status));
        }
        let mut buffer = vec![0u8; 4096];
        // SAFETY: `target` was filled by the move; `buffer` is writable for its length.
        let status = unsafe { FSRefMakePath(&target, buffer.as_mut_ptr(), buffer.len() as u32) };
        let trashed_as = if status == NO_ERR {
            let len = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
            buffer.truncate(len);
            Some(PathBuf::from(OsString::from_vec(buffer)))
        } else {
            None
        };
        Ok(Moved {
            trashed_as,
            trashed_unix_ms: None,
        })
    }

    fn change_time_ms(meta: &std::fs::Metadata) -> u64 {
        (meta.ctime().max(0) as u64) * 1000 + (meta.ctime_nsec().max(0) as u64) / 1_000_000
    }

    /// The directories that hold the current user's trash: `~/.Trash` and each mounted volume's
    /// `.Trashes/<uid>`. With `volume_of`, only the trash of that path's volume and the home trash.
    fn trash_dirs(volume_of: Option<&Path>) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            dirs.push(PathBuf::from(home).join(".Trash"));
        }
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let volumes = Path::new("/Volumes");
        match volume_of {
            Some(path) => {
                if let Ok(rest) = path.strip_prefix(volumes) {
                    if let Some(name) = rest.components().next() {
                        dirs.push(
                            volumes
                                .join(name.as_os_str())
                                .join(".Trashes")
                                .join(uid.to_string()),
                        );
                    }
                }
            }
            None => {
                if let Ok(entries) = std::fs::read_dir(volumes) {
                    for entry in entries.flatten() {
                        dirs.push(entry.path().join(".Trashes").join(uid.to_string()));
                    }
                }
            }
        }
        dirs
    }

    fn entries_in(dir: &Path) -> Vec<TrashEntry> {
        let Ok(read) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in read.flatten() {
            if entry.file_name() == ".DS_Store" {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            out.push(TrashEntry {
                path,
                key: FileKey {
                    volume: meta.dev(),
                    index: meta.ino(),
                },
                original: None,
                trashed_unix_ms: change_time_ms(&meta),
            });
        }
        out
    }

    pub fn list() -> Result<Vec<TrashEntry>> {
        let mut all = Vec::new();
        for dir in trash_dirs(None) {
            all.extend(entries_in(&dir));
        }
        Ok(all)
    }

    pub fn lookup(identity: &TrashIdentity) -> Result<Option<TrashEntry>> {
        let matches = |entry: &TrashEntry| {
            entry.key == identity.key
                && entry_time_matches(entry.trashed_unix_ms, identity.trashed_unix_ms)
        };
        if let Some(hint) = &identity.trashed_as {
            if let Some(parent) = hint.parent() {
                if let Some(found) = entries_in(parent)
                    .into_iter()
                    .find(|e| e.path == *hint && matches(e))
                {
                    return Ok(Some(found));
                }
            }
        }
        for dir in trash_dirs(Some(&identity.original)) {
            if let Some(found) = entries_in(&dir).into_iter().find(|e| matches(e)) {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    pub fn restore(entry: &TrashEntry, identity: &TrashIdentity) -> Result<()> {
        crate::atomic::rename_exclusive(&entry.path, &identity.original)
    }
}

#[cfg(windows)]
mod sys {
    use super::{parse_recycle_info, FileKey, FsError, Moved, Result, TrashEntry, TrashIdentity};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Component, Path, PathBuf, Prefix};

    const DRIVE_FIXED: u32 = 3;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x7;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    /// Largest `$I` record read: a version 2 record with a 32 768-character path is under 66 KiB.
    const MAX_RECORD_BYTES: u64 = 128 * 1024;
    /// How far apart two readings of one deletion time may be.
    const TIME_TOLERANCE_MS: u64 = 2_000;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDriveTypeW(root: *const u16) -> u32;
        fn GetLogicalDrives() -> u32;
    }

    pub fn key_nofollow(path: &Path) -> Result<FileKey> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ_WRITE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|e| FsError::io(path, e))?;
        FileKey::of_file(&file, path)
    }

    /// The drive letter of a plain `X:\...` path, or why the trash cannot be used for the path.
    fn drive_of(path: &Path) -> Result<char> {
        let plain = crate::plain::plain_path(path.to_path_buf());
        match plain.components().next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(letter) => Ok(char::from(letter).to_ascii_uppercase()),
                _ => Err(FsError::Unavailable(
                    "the Recycle Bin needs a path on a drive with a letter (no UNC or device paths)",
                )),
            },
            _ => Err(FsError::Unavailable("the Recycle Bin needs an absolute drive path")),
        }
    }

    fn root_wide(drive: char) -> Vec<u16> {
        format!("{drive}:\\")
            .encode_utf16()
            .chain(Some(0))
            .collect()
    }

    fn require_fixed_drive(drive: char) -> Result<()> {
        // SAFETY: NUL-terminated root path.
        let kind = unsafe { GetDriveTypeW(root_wide(drive).as_ptr()) };
        if kind == DRIVE_FIXED {
            Ok(())
        } else {
            Err(FsError::Unavailable(
                "the Recycle Bin is only used for fixed drives (not network or removable ones)",
            ))
        }
    }

    fn bin_dir(drive: char) -> Result<PathBuf> {
        let probe = PathBuf::from(format!("{drive}:\\"));
        let sid = crate::private::current_process_sid_string(&probe)?;
        Ok(PathBuf::from(format!("{drive}:\\$Recycle.Bin\\{sid}")))
    }

    struct Record {
        data: PathBuf,
        key: FileKey,
        original: String,
        deleted_unix_ms: u64,
    }

    fn records(drive: char) -> Result<Vec<Record>> {
        let bin = bin_dir(drive)?;
        let read = match std::fs::read_dir(&bin) {
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(FsError::io(&bin, e)),
        };
        let mut out = Vec::new();
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_prefix("$I") else {
                continue;
            };
            let info = entry.path();
            match std::fs::metadata(&info) {
                Ok(meta) if meta.is_file() && meta.len() <= MAX_RECORD_BYTES => {}
                _ => continue,
            }
            let Ok(bytes) = std::fs::read(&info) else {
                continue;
            };
            let Some(parsed) = parse_recycle_info(&bytes) else {
                continue;
            };
            let data = bin.join(format!("$R{id}"));
            let Ok(key) = key_nofollow(&data) else {
                continue; // a record whose data is gone
            };
            out.push(Record {
                data,
                key,
                original: parsed.original,
                deleted_unix_ms: parsed.deleted_unix_ms,
            });
        }
        Ok(out)
    }

    fn entry_of(record: Record) -> TrashEntry {
        TrashEntry {
            path: record.data,
            key: record.key,
            original: Some(PathBuf::from(record.original)),
            trashed_unix_ms: record.deleted_unix_ms,
        }
    }

    fn same_path(a: &str, b: &Path) -> bool {
        a.to_lowercase() == b.to_string_lossy().to_lowercase()
    }

    /// Refuses without touching the item: `SHFileOperationW` cannot be made recycle-only (with no
    /// confirmation it deletes permanently when the item cannot be recycled), and no recycle-only
    /// API is bound here. Fail closed rather than risk irreversible deletion.
    pub fn move_item(_original: &Path, _key: FileKey) -> Result<Moved> {
        Err(FsError::Unavailable(
            "moving to the Recycle Bin is unavailable: the shell would delete permanently when an \
             item cannot be recycled, and no recycle-only operation is bound",
        ))
    }

    pub fn list() -> Result<Vec<TrashEntry>> {
        let mut all = Vec::new();
        // SAFETY: no arguments.
        let mask = unsafe { GetLogicalDrives() };
        for index in 0..26u32 {
            if mask & (1 << index) == 0 {
                continue;
            }
            let drive = char::from(b'A' + index as u8);
            if require_fixed_drive(drive).is_err() {
                continue;
            }
            all.extend(records(drive)?.into_iter().map(entry_of));
        }
        Ok(all)
    }

    pub fn lookup(identity: &TrashIdentity) -> Result<Option<TrashEntry>> {
        let drive = drive_of(&identity.original)?;
        let found = records(drive)?.into_iter().find(|r| {
            r.key == identity.key
                && same_path(&r.original, &identity.original)
                && r.deleted_unix_ms.abs_diff(identity.trashed_unix_ms) <= TIME_TOLERANCE_MS
        });
        Ok(found.map(entry_of))
    }

    pub fn restore(entry: &TrashEntry, identity: &TrashIdentity) -> Result<()> {
        crate::atomic::rename_exclusive(&entry.path, &identity.original)?;
        // Best effort: drop the `$I` record that described the item. Its name is the data name with
        // `$R` swapped for `$I`.
        if let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) {
            if let Some(id) = name.strip_prefix("$R") {
                if let Some(dir) = entry.path.parent() {
                    let _ = std::fs::remove_file(dir.join(format!("$I{id}")));
                }
            }
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod sys {
    use super::{unavailable, FileKey, Moved, Result, TrashEntry, TrashIdentity};
    use std::path::Path;

    pub fn key_nofollow(_path: &Path) -> Result<FileKey> {
        Err(unavailable())
    }
    pub fn move_item(_original: &Path, _key: FileKey) -> Result<Moved> {
        Err(unavailable())
    }
    pub fn list() -> Result<Vec<TrashEntry>> {
        Err(unavailable())
    }
    pub fn lookup(_identity: &TrashIdentity) -> Result<Option<TrashEntry>> {
        Err(unavailable())
    }
    pub fn restore(_entry: &TrashEntry, _identity: &TrashIdentity) -> Result<()> {
        Err(unavailable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_v2(path: &str, filetime: u64, size: u64) -> Vec<u8> {
        let units: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let mut out = Vec::new();
        out.extend_from_slice(&2u64.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&filetime.to_le_bytes());
        out.extend_from_slice(&(units.len() as u32).to_le_bytes());
        for unit in units {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    fn record_v1(path: &str, filetime: u64, size: u64) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&1u64.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&filetime.to_le_bytes());
        let mut field = vec![0u8; 520];
        for (i, unit) in path.encode_utf16().enumerate() {
            field[i * 2..i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&field);
        out
    }

    /// 2026-01-01T00:00:00Z as a FILETIME.
    const FILETIME_2026: u64 = (1_767_225_600_000 + 11_644_473_600_000) * 10_000;

    #[test]
    fn filetime_converts_to_unix_milliseconds() {
        assert_eq!(filetime_to_unix_ms(FILETIME_2026), 1_767_225_600_000);
        assert_eq!(filetime_to_unix_ms(116_444_736_000_000_000), 0);
        assert_eq!(filetime_to_unix_ms(0), 0, "before 1970 saturates");
        assert_eq!(
            filetime_to_unix_ms(u64::MAX),
            u64::MAX / 10_000 - 11_644_473_600_000
        );
    }

    #[test]
    fn a_version_2_record_parses() {
        let bytes = record_v2(r"C:\Users\me\Documents\report.docx", FILETIME_2026, 4096);
        let info = parse_recycle_info(&bytes).unwrap();
        assert_eq!(info.original, r"C:\Users\me\Documents\report.docx");
        assert_eq!(info.size, 4096);
        assert_eq!(info.deleted_unix_ms, 1_767_225_600_000);
    }

    #[test]
    fn a_version_1_record_parses_with_its_fixed_field() {
        let path = "D:\\data\\caf\u{e9}.txt";
        let bytes = record_v1(path, FILETIME_2026, 7);
        assert_eq!(bytes.len(), 544);
        let info = parse_recycle_info(&bytes).unwrap();
        assert_eq!(info.original, path);
        assert_eq!(info.size, 7);
    }

    #[test]
    fn non_ascii_paths_round_trip() {
        let path = "C:\\Users\\\u{e9}l\u{e8}ve\\\u{4e2d}\u{6587}.txt";
        let info = parse_recycle_info(&record_v2(path, FILETIME_2026, 1)).unwrap();
        assert_eq!(info.original, path);
    }

    #[test]
    fn malformed_records_are_none() {
        let good = record_v2(r"C:\x", FILETIME_2026, 1);
        assert!(parse_recycle_info(&good).is_some());
        // Truncated at every length shorter than the header or the path.
        for len in 0..good.len() {
            assert!(parse_recycle_info(&good[..len]).is_none(), "len {len}");
        }
        // Unknown version.
        let mut bad = good.clone();
        bad[0] = 3;
        assert!(parse_recycle_info(&bad).is_none());
        // A length field larger than the data, zero, and absurd.
        for chars in [0u32, 1000, u32::MAX, 40_000] {
            let mut bad = good.clone();
            bad[24..28].copy_from_slice(&chars.to_le_bytes());
            assert!(parse_recycle_info(&bad).is_none(), "chars {chars}");
        }
        // An empty path and a lone surrogate.
        assert!(parse_recycle_info(&record_v2("", FILETIME_2026, 1)).is_none());
        let mut lone = Vec::new();
        lone.extend_from_slice(&2u64.to_le_bytes());
        lone.extend_from_slice(&0u64.to_le_bytes());
        lone.extend_from_slice(&FILETIME_2026.to_le_bytes());
        lone.extend_from_slice(&2u32.to_le_bytes());
        lone.extend_from_slice(&0xD800u16.to_le_bytes());
        lone.extend_from_slice(&0u16.to_le_bytes());
        assert!(parse_recycle_info(&lone).is_none());
    }

    #[test]
    fn the_entry_time_window_rejects_an_older_entry_that_reused_an_inode() {
        let trashed = 10_000_000;
        assert!(entry_time_matches(trashed, trashed));
        assert!(entry_time_matches(trashed - WINDOW_MS, trashed));
        assert!(!entry_time_matches(trashed - WINDOW_MS - 1, trashed));
        assert!(
            entry_time_matches(trashed + 5_000_000, trashed),
            "touched later"
        );
        assert!(!entry_time_matches(0, trashed));
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn other_platforms_report_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        assert!(matches!(move_to_trash(&file), Err(FsError::Unavailable(_))));
        assert!(file.exists(), "nothing was removed");
        assert!(matches!(list(), Err(FsError::Unavailable(_))));
    }

    /// The Windows move never reaches a call that can delete permanently: it refuses first, for a
    /// small file and for a path that does not exist alike, and the file stays where it was.
    #[cfg(windows)]
    #[test]
    fn windows_refuses_to_trash_and_leaves_the_item_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("keep.txt");
        std::fs::write(&file, b"must survive").unwrap();
        let err = move_to_trash(&file).unwrap_err();
        assert!(matches!(err, FsError::Unavailable(_)), "{err:?}");
        assert_eq!(std::fs::read(&file).unwrap(), b"must survive");
        let tree = dir.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("a"), b"a").unwrap();
        assert!(matches!(move_to_trash(&tree), Err(FsError::Unavailable(_))));
        assert!(tree.join("a").exists());
    }

    #[test]
    fn a_path_with_a_nul_byte_is_refused_before_anything_happens() {
        let err = move_to_trash(Path::new("/tmp/a\0b")).unwrap_err();
        assert!(matches!(err, FsError::BadName(_)), "{err:?}");
    }

    #[test]
    fn a_missing_path_is_a_not_found_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = move_to_trash(&dir.path().join("absent")).unwrap_err();
        assert!(
            err.is_not_found() || matches!(err, FsError::Unavailable(_)),
            "{err:?}"
        );
    }

    /// Moves a real file to the real trash and back. macOS only: it touches the user's Trash, so it
    /// removes exactly the entry it made and nothing else.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_file_goes_to_the_trash_is_found_and_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir
            .path()
            .join(format!("rightkit-trash-test-{}.txt", std::process::id()));
        std::fs::write(&file, b"restore me").unwrap();
        let identity = move_to_trash(&file).unwrap();
        assert!(!file.exists(), "the file left its place");
        let entry = lookup(&identity).unwrap().expect("found by identity");
        assert_eq!(entry.key, identity.key);
        assert!(list().unwrap().iter().any(|e| e.path == entry.path));
        // A new file at the original path blocks the restore and is not overwritten.
        std::fs::write(&file, b"newcomer").unwrap();
        let err = restore(&identity).unwrap_err();
        assert!(matches!(err, FsError::AlreadyExists(_)), "{err:?}");
        assert_eq!(std::fs::read(&file).unwrap(), b"newcomer");
        std::fs::remove_file(&file).unwrap();
        assert_eq!(restore(&identity).unwrap(), file);
        assert_eq!(std::fs::read(&file).unwrap(), b"restore me");
        assert!(
            lookup(&identity).unwrap().is_none(),
            "no longer in the trash"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_identity_with_the_wrong_key_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir
            .path()
            .join(format!("rightkit-trash-key-{}.txt", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let mut identity = move_to_trash(&file).unwrap();
        let real = identity.clone();
        identity.key = FileKey {
            volume: identity.key.volume,
            index: identity.key.index ^ 0x5555,
        };
        identity.trashed_as = None;
        assert!(lookup(&identity).unwrap().is_none());
        // Clean up: put the real one back and remove it.
        restore(&real).unwrap();
        std::fs::remove_file(&file).unwrap();
    }
}
