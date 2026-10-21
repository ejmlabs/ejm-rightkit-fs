//! Questions you can ask an open handle, and a no-follow read open.
//!
//! - [`final_path`]: where the OS says an open file really lives (`F_GETPATH` on macOS,
//!   `/proc/self/fd` on Linux, `GetFinalPathNameByHandleW` on Windows). The recheck after an open
//!   that must stay inside a folder: whatever redirected the path on the way (a symlink, a junction,
//!   a swapped directory), the handle's own path is the truth.
//! - [`link_count`]: how many hard links the file has. A file with more than one is reachable by
//!   another name, possibly outside the folder it was found in.
//! - [`open_read_nofollow`] and [`ReadOptions`]: open a regular file for reading without following
//!   a symlink or reparse point at the final component, optionally requiring a single hard link,
//!   owner-only access, and (Windows) refusing writers while it is open.
//!
//! A file's identity (volume and index, on every platform, directories included) is
//! [`crate::FileKey`].
//!
//! # What the no-follow open does and does not pin
//!
//! Only the final component is protected: a symlink or junction in a parent directory is followed
//! by the operating system. To pin the parents too, open through [`crate::StateDir`]
//! (descriptor-relative on Unix, handle-relative on Windows) or through
//! [`crate::contain::Roots::open_confined`] (walks from an approved root and rechecks the handle's
//! final path).

use crate::error::{FsError, Result};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// How a no-follow read open treats the file it finds. `ReadOptions::default()` is a plain
/// regular-file open that refuses links at the final component and checks nothing else.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadOptions {
    /// Refuse a file with more than one hard link ([`FsError::HardLinked`]). Closes the case where
    /// a file inside an approved folder is a hard link to one outside it (a hard link cannot be told
    /// from a first name by looking at the path). The check is made on the open handle, so it
    /// describes the file that was opened. A hard link made after the open is not seen.
    pub require_single_link: bool,
    /// Verify ownership and private access on this open handle, without changing permissions.
    /// Unix requires the effective user as owner, no group/other mode bits, and on macOS no ACL
    /// `allow` entry. Windows requires the protected single-user DACL this crate creates (the
    /// strict predicate documented by [`crate::private::is_owner_only_strict`]). Unsupported
    /// platforms fail closed. This is a snapshot: later permission changes are not prevented.
    pub require_owner_only: bool,
    /// Windows: open with `FILE_SHARE_READ` only, so while the handle is open nobody else can open
    /// the file for writing, deleting or renaming it, and the open itself fails with a sharing
    /// violation while somebody else holds it open for write. The bytes cannot change under the
    /// reader. Unix has no mandatory sharing and ignores this flag (an advisory lock would bind
    /// only cooperating processes).
    pub share_read_only: bool,
}

impl ReadOptions {
    /// No extra checks.
    pub const fn new() -> Self {
        Self {
            require_single_link: false,
            require_owner_only: false,
            share_read_only: false,
        }
    }

    /// Require exactly one hard link; see [`ReadOptions::require_single_link`].
    pub const fn require_single_link(mut self, yes: bool) -> Self {
        self.require_single_link = yes;
        self
    }

    /// Verify private ownership and access; see [`ReadOptions::require_owner_only`].
    pub const fn require_owner_only(mut self, yes: bool) -> Self {
        self.require_owner_only = yes;
        self
    }

    /// Windows read-sharing only; see [`ReadOptions::share_read_only`].
    pub const fn share_read_only(mut self, yes: bool) -> Self {
        self.share_read_only = yes;
        self
    }
}

/// The path the operating system reports for an open file.
///
/// macOS asks the descriptor (`fcntl(F_GETPATH)`), Linux and Android read `/proc/self/fd/N`, Windows
/// calls `GetFinalPathNameByHandleW` (normalized DOS name; a verbatim `\\?\C:` prefix is reduced to
/// `C:` as [`crate::plain_path`] does). Other targets return [`io::ErrorKind::Unsupported`]. On
/// Linux a file whose last name was removed reports its old path with a ` (deleted)` suffix. The
/// answer is a snapshot: the file can be renamed a moment later.
pub fn final_path(file: &File) -> io::Result<PathBuf> {
    final_path_impl(file)
}

#[cfg(target_vendor = "apple")]
fn final_path_impl(file: &File) -> io::Result<PathBuf> {
    use std::ffi::OsStr;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    // MAXPATHLEN, the size `F_GETPATH` documents for its buffer.
    let mut buffer = [0u8; 1024];
    // SAFETY: the descriptor is open and `buffer` is the 1024 bytes F_GETPATH requires.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    let len = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
    Ok(PathBuf::from(OsStr::from_bytes(&buffer[..len])))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn final_path_impl(file: &File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(all(
    unix,
    not(any(target_vendor = "apple", target_os = "linux", target_os = "android"))
))]
fn final_path_impl(_file: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the final path of a handle is not available on this platform",
    ))
}

#[cfg(windows)]
fn final_path_impl(file: &File) -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            handle: *mut std::ffi::c_void,
            buffer: *mut u16,
            len: u32,
            flags: u32,
        ) -> u32;
    }
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: the handle is a live file handle and `buf` is valid for `buf.len()` units.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle().cast(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                0, // VOLUME_NAME_DOS | FILE_NAME_NORMALIZED
            )
        };
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if (n as usize) < buf.len() {
            buf.truncate(n as usize);
            return Ok(crate::plain::plain_path(PathBuf::from(
                OsString::from_wide(&buf),
            )));
        }
        // Too small: `n` is the size needed including the terminator.
        buf.resize(n as usize + 1, 0);
    }
}

#[cfg(not(any(unix, windows)))]
fn final_path_impl(_file: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the final path of a handle is not available on this platform",
    ))
}

/// The number of hard links to the open file (`st_nlink` on Unix, `nNumberOfLinks` from
/// `GetFileInformationByHandle` on Windows). A directory's count on Unix is not 1 and means nothing
/// here; ask about files.
pub fn link_count(file: &File) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(file.metadata()?.nlink())
    }
    #[cfg(windows)]
    {
        crate::file_key::windows_info(file).map(|(_, links)| u64::from(links))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "link counts are not available on this platform",
        ))
    }
}

/// Checks made on a freshly opened read handle, shared by every no-follow read open: a regular file,
/// and one hard link and owner-only access when asked. `shown` names the entry in errors.
pub(crate) fn finish_read_open(file: File, shown: &Path, options: &ReadOptions) -> Result<File> {
    let meta = file.metadata().map_err(|e| FsError::io(shown, e))?;
    if !meta.is_file() {
        return Err(FsError::NotAFile(shown.to_path_buf()));
    }
    #[cfg(unix)]
    clear_nonblock(&file).map_err(|e| FsError::io(shown, e))?;
    if options.require_single_link && link_count(&file).map_err(|e| FsError::io(shown, e))? != 1 {
        return Err(FsError::HardLinked(shown.to_path_buf()));
    }
    if options.require_owner_only {
        crate::private::verify_owner_only_handle(&file, shown)?;
    }
    Ok(file)
}

/// The no-follow opens add `O_NONBLOCK` so a planted FIFO or device cannot hang the open; a regular
/// file is then handed back in the ordinary blocking mode.
#[cfg(unix)]
pub(crate) fn clear_nonblock(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor is open; F_GETFL and F_SETFL take no pointers.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if flags & libc::O_NONBLOCK != 0
            && libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) == -1
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Open the regular file at `path` for reading without following a symlink or reparse point at the
/// final component, and check the opened handle ([`ReadOptions`]).
///
/// A link at the final component is [`FsError::Symlink`] (Unix `O_NOFOLLOW`; Windows
/// `FILE_FLAG_OPEN_REPARSE_POINT`, then the handle's reparse type is checked, so the link itself is
/// opened and refused, never its target). Anything that is not a regular file (a directory, a FIFO,
/// a device) is [`FsError::NotAFile`]; a FIFO is never waited on. A file with more than one hard link
/// is [`FsError::HardLinked`] when `require_single_link` is set. See the module docs for what this
/// does not pin (parent directories).
pub fn open_read_nofollow(path: &Path, options: &ReadOptions) -> Result<File> {
    let file = open_leaf(path, options)?;
    finish_read_open(file, path, options)
}

#[cfg(unix)]
fn open_leaf(path: &Path, _options: &ReadOptions) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            Err(FsError::Symlink(path.to_path_buf()))
        }
        Err(e) => Err(FsError::io(path, e)),
    }
}

#[cfg(windows)]
fn open_leaf(path: &Path, options: &ReadOptions) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    /// Open the reparse point itself instead of following it.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x7;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(if options.share_read_only {
            FILE_SHARE_READ
        } else {
            FILE_SHARE_READ_WRITE_DELETE
        })
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|e| FsError::io(path, e))?;
    let meta = file.metadata().map_err(|e| FsError::io(path, e))?;
    if meta.file_type().is_symlink() {
        return Err(FsError::Symlink(path.to_path_buf()));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_leaf(path: &Path, _options: &ReadOptions) -> Result<File> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(FsError::Symlink(path.to_path_buf()));
        }
    }
    File::open(path).map_err(|e| FsError::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_in(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_regular_file_opens_and_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path(), "f", b"hello");
        let mut file = open_read_nofollow(&path, &ReadOptions::new()).unwrap();
        let mut text = String::new();
        io::Read::read_to_string(&mut file, &mut text).unwrap();
        assert_eq!(text, "hello");
    }

    #[test]
    fn a_missing_file_is_a_not_found_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_read_nofollow(&dir.path().join("absent"), &ReadOptions::new()).unwrap_err();
        assert!(err.is_not_found(), "{err:?}");
    }

    #[test]
    fn a_directory_is_not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_read_nofollow(dir.path(), &ReadOptions::new()).unwrap_err();
        assert!(
            matches!(err, FsError::NotAFile(_) | FsError::Io { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_nul_byte_in_the_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_read_nofollow(&dir.path().join("a\0b"), &ReadOptions::new()).unwrap_err();
        assert!(matches!(err, FsError::Io { .. }), "{err:?}");
    }

    #[test]
    fn link_count_counts_hard_links() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path(), "f", b"x");
        assert_eq!(link_count(&File::open(&path).unwrap()).unwrap(), 1);
        std::fs::hard_link(&path, dir.path().join("g")).unwrap();
        assert_eq!(link_count(&File::open(&path).unwrap()).unwrap(), 2);
    }

    #[test]
    fn a_hard_linked_file_is_refused_only_when_a_single_link_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path(), "f", b"x");
        let strict = ReadOptions::new().require_single_link(true);
        assert!(open_read_nofollow(&path, &strict).is_ok());
        std::fs::hard_link(&path, dir.path().join("alias")).unwrap();
        let err = open_read_nofollow(&path, &strict).unwrap_err();
        assert!(matches!(err, FsError::HardLinked(_)), "{err:?}");
        assert!(open_read_nofollow(&path, &ReadOptions::new()).is_ok());
    }

    #[cfg(any(target_vendor = "apple", target_os = "linux", windows))]
    #[test]
    fn final_path_names_the_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path(), "real.txt", b"x");
        let file = File::open(&path).unwrap();
        let want = crate::plain::canonicalize_plain(&path).unwrap();
        let got = final_path(&file).unwrap();
        assert_eq!(
            got.to_string_lossy().to_lowercase(),
            want.to_string_lossy().to_lowercase()
        );
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::symlink;

        #[test]
        fn a_symlink_at_the_final_component_is_refused_even_when_it_points_inside() {
            let dir = tempfile::tempdir().unwrap();
            let target = file_in(dir.path(), "target", b"secret");
            symlink(&target, dir.path().join("link")).unwrap();
            let err =
                open_read_nofollow(&dir.path().join("link"), &ReadOptions::new()).unwrap_err();
            assert!(matches!(err, FsError::Symlink(_)), "{err:?}");
        }

        #[test]
        fn a_dangling_symlink_is_a_symlink_error() {
            let dir = tempfile::tempdir().unwrap();
            symlink(dir.path().join("nowhere"), dir.path().join("dangling")).unwrap();
            let err =
                open_read_nofollow(&dir.path().join("dangling"), &ReadOptions::new()).unwrap_err();
            assert!(matches!(err, FsError::Symlink(_)), "{err:?}");
        }

        #[test]
        fn a_fifo_is_refused_without_blocking() {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("pipe");
            let c = CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: valid NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            let err = open_read_nofollow(&path, &ReadOptions::new()).unwrap_err();
            assert!(matches!(err, FsError::NotAFile(_)), "{err:?}");
        }

        #[test]
        fn the_returned_handle_is_blocking_again() {
            use std::os::fd::AsRawFd;
            let dir = tempfile::tempdir().unwrap();
            let path = file_in(dir.path(), "f", b"x");
            let file = open_read_nofollow(&path, &ReadOptions::new()).unwrap();
            // SAFETY: the descriptor is open; F_GETFL takes no pointer.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
            assert_eq!(flags & libc::O_NONBLOCK, 0);
        }
    }

    #[cfg(windows)]
    mod windows {
        use super::*;

        #[test]
        fn share_read_only_refuses_a_concurrent_writer_in_both_directions() {
            let dir = tempfile::tempdir().unwrap();
            let path = file_in(dir.path(), "f", b"x");
            let shared = ReadOptions::new().share_read_only(true);
            let reader = open_read_nofollow(&path, &shared).unwrap();
            let writer = std::fs::OpenOptions::new().write(true).open(&path);
            assert_eq!(
                writer.unwrap_err().raw_os_error(),
                Some(32),
                "sharing violation"
            );
            drop(reader);
            let writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            let err = open_read_nofollow(&path, &shared).unwrap_err();
            assert!(
                matches!(&err, FsError::Io { source, .. } if source.raw_os_error() == Some(32)),
                "{err:?}"
            );
            drop(writer);
        }

        #[test]
        fn a_symlink_or_junction_at_the_final_component_is_refused() {
            let dir = tempfile::tempdir().unwrap();
            let target = file_in(dir.path(), "target", b"x");
            let link = dir.path().join("link");
            // Creating a symlink needs a privilege or Developer Mode; skip the check without it.
            if std::os::windows::fs::symlink_file(&target, &link).is_ok() {
                let err = open_read_nofollow(&link, &ReadOptions::new()).unwrap_err();
                assert!(matches!(err, FsError::Symlink(_)), "{err:?}");
            }
        }
    }
}
