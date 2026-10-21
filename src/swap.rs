//! Atomic exchange of two paths.
//!
//! On macOS this calls `renamex_np(a, b, RENAME_SWAP)`: the kernel exchanges the two directory
//! entries in one step, so each name always resolves to a complete tree, even if the process is
//! killed mid-call. Both paths must exist and live on one APFS volume. Filesystems without
//! `RENAME_SWAP` (HFS+, many network and FAT volumes) fail with `ENOTSUP` or `EINVAL`, which
//! [`is_swap_unsupported`] recognises so callers can fall back to a two-rename sequence.
//! Other platforms always return [`std::io::ErrorKind::Unsupported`].

use std::io;
use std::path::Path;

/// Atomically exchanges the filesystem objects at `a` and `b`.
///
/// Both must exist. On success `a` names what `b` named and the reverse; inodes are preserved.
/// On failure nothing changed.
#[cfg(target_os = "macos")]
pub fn swap_paths(a: &Path, b: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_a = CString::new(a.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let c_b = CString::new(b.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: both pointers are valid NUL-terminated strings that outlive the call; the flag is a documented value.
    let rc = unsafe { libc::renamex_np(c_a.as_ptr(), c_b.as_ptr(), libc::RENAME_SWAP) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Non-macOS platforms have no `RENAME_SWAP`; callers must fall back or fail closed.
#[cfg(not(target_os = "macos"))]
pub fn swap_paths(_a: &Path, _b: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic path swap is macOS-only",
    ))
}

/// True when `error` means the volume or platform cannot swap atomically (`ENOTSUP`, `EINVAL`,
/// `ENOSYS`, or [`io::ErrorKind::Unsupported`]), as opposed to a real failure such as a missing path.
pub fn is_swap_unsupported(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        return code == libc::ENOTSUP || code == libc::EINVAL || code == libc::ENOSYS;
    }
    false
}
