//! Free space on the volume that holds a path.
//!
//! [`free_bytes`] asks the OS directly: `statvfs(3)` on unix, `GetDiskFreeSpaceExW` on Windows. No
//! child process is started (an app that shells out to `df -k` pays for a process and parses text
//! that changes between platforms).

use std::io;
use std::path::Path;

/// Bytes available to the calling user on the volume that holds `path`.
///
/// `path` must exist (on unix it is handed to `statvfs`, which needs an existing entry; on Windows
/// any existing path on the volume works). This is the space the user may write, which can be
/// less than the volume's total free space when the filesystem reserves blocks for root or quotas
/// apply. Other platforms return [`io::ErrorKind::Unsupported`].
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: `statvfs` is plain old data; every field is an integer, so all-zero is a valid value
    // that the call then overwrites on success.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call, and `stat` is a
    // valid out-parameter.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // `f_bavail` counts fragment-size blocks available to unprivileged users.
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// Bytes available to the calling user on the volume that holds `path` (`GetDiskFreeSpaceExW`).
#[cfg(windows)]
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_to_caller: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_to_caller: u64 = 0;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the optional out-parameters are
    // null, which the API accepts; `free_to_caller` is a valid out-parameter.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(free_to_caller)
}

/// Other platforms have no free-space query in this crate.
#[cfg(not(any(unix, windows)))]
pub fn free_bytes(_path: &Path) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "free_bytes is implemented for unix and Windows",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_existing_directory_reports_some_space() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = free_bytes(dir.path()).expect("the temp volume answers");
        assert!(bytes > 0);
    }

    #[test]
    fn a_missing_path_is_an_error_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_bytes(&dir.path().join("absent-space-probe")).is_err());
    }
}
