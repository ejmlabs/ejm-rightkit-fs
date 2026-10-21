//! The filesystem's own identity for a file: [`FileKey`] (device and inode on unix, volume serial
//! and file index on Windows).
//!
//! Compiled with feature `fs` alone. The stable-identity ledger in [`crate::identity`] needs
//! feature `json` on top of it; a consumer that only compares two files does not.

use crate::error::{FsError, Result};
use std::fs::{File, OpenOptions};
use std::path::Path;

/// The filesystem's own identity for an open file. Serialized (with feature `json`) as the same
/// two numbers the identity ledger stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "json", derive(serde::Serialize, serde::Deserialize))]
pub struct FileKey {
    pub volume: u64,
    pub index: u64,
}

/// `FILE_FLAG_BACKUP_SEMANTICS`: lets `CreateFileW` open a directory. Without it a directory
/// cannot be opened for its identity and the open fails with access denied.
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

/// Opens `path` read-only for its identity. On Windows the handle may be a directory's (the
/// backup-semantics flag); on unix an ordinary read-only open already works for a directory.
fn open_for_identity(path: &Path) -> std::io::Result<File> {
    let mut open = OpenOptions::new();
    open.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        open.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    open.open(path)
}

impl FileKey {
    /// The identity of the file or directory at `path`, following a symlink at `path` to its target
    /// as `File::open` does. A directory works on every platform.
    pub fn of_path(path: &Path) -> Result<Self> {
        let file = open_for_identity(path).map_err(|e| FsError::io(path, e))?;
        Self::of_file(&file, path)
    }

    pub fn of_file(file: &File, shown: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = file.metadata().map_err(|e| FsError::io(shown, e))?;
            Ok(Self {
                volume: meta.dev(),
                index: meta.ino(),
            })
        }
        #[cfg(windows)]
        {
            windows_key(file).map_err(|e| FsError::io(shown, e))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (file, shown);
            Err(FsError::Unavailable("file identity"))
        }
    }

    // Reached from `identity::resolve` only on feature sets that track by metadata.
    #[allow(dead_code)]
    pub(crate) fn of_meta(meta: &std::fs::Metadata, path: &Path) -> Option<Self> {
        #[cfg(unix)]
        {
            let _ = path;
            use std::os::unix::fs::MetadataExt;
            Some(Self {
                volume: meta.dev(),
                index: meta.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            Self::of_path(path).ok()
        }
    }
}

/// The identity and hard-link count of an open handle (`GetFileInformationByHandle`).
#[cfg(windows)]
pub(crate) fn windows_info(file: &File) -> std::io::Result<(FileKey, u32)> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct ByHandleFileInformation {
        attributes: u32,
        creation: FileTime,
        access: FileTime,
        write: FileTime,
        volume_serial: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut std::ffi::c_void,
            info: *mut ByHandleFileInformation,
        ) -> i32;
    }
    let mut info = std::mem::MaybeUninit::<ByHandleFileInformation>::zeroed();
    // SAFETY: the handle is a live file handle and `info` is a valid out-parameter.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), info.as_mut_ptr()) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the call succeeded and filled the struct.
    let info = unsafe { info.assume_init() };
    Ok((
        FileKey {
            volume: u64::from(info.volume_serial),
            index: (u64::from(info.index_high) << 32) | u64::from(info.index_low),
        },
        info.links,
    ))
}

#[cfg(windows)]
fn windows_key(file: &File) -> std::io::Result<FileKey> {
    windows_info(file).map(|(key, _links)| key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_has_an_identity_and_it_matches_its_handle() {
        let dir = tempfile::tempdir().unwrap();
        let by_path = FileKey::of_path(dir.path()).expect("a directory opens for its identity");
        // A directory handle needs the backup-semantics flag on Windows; `File::open` is denied there.
        let handle = open_for_identity(dir.path()).unwrap();
        assert_eq!(FileKey::of_file(&handle, dir.path()).unwrap(), by_path);
    }

    #[test]
    fn a_file_and_its_sibling_directory_differ() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        assert_ne!(
            FileKey::of_path(&file).unwrap(),
            FileKey::of_path(dir.path()).unwrap()
        );
    }

    #[test]
    fn a_missing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(FileKey::of_path(&dir.path().join("absent")).is_err());
    }
}
