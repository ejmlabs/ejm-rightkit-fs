//! Is a path on a network or remote filesystem? SQLite WAL (shared-memory locking) and OS
//! advisory locks are unreliable there, so callers pick rollback journaling / refuse to rely on
//! lock files. Donor: CodeRight `private_fs::journal` (statfs / UNC / OneDrive detection).

use std::path::Path;

/// Conservative: `true` only when the filesystem is positively identified as remote.
pub fn is_network_filesystem(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CStr;
        use std::os::unix::ffi::OsStrExt;
        let probe = nearest_existing(path);
        let Ok(c) = std::ffi::CString::new(probe.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: zeroed statfs is a valid out-parameter and `c` is NUL-terminated.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
            return false;
        }
        // SAFETY: f_fstypename is a NUL-terminated array filled by the kernel.
        let name = unsafe { CStr::from_ptr(st.f_fstypename.as_ptr()) }
            .to_string_lossy()
            .to_ascii_lowercase();
        matches!(
            name.as_str(),
            "nfs" | "smbfs" | "afpfs" | "webdav" | "cifs" | "ftp" | "osxfusefs" | "macfuse"
        )
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::os::unix::ffi::OsStrExt;
        let probe = nearest_existing(path);
        let Ok(c) = std::ffi::CString::new(probe.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: zeroed statfs is a valid out-parameter and `c` is NUL-terminated.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
            return false;
        }
        const NFS: i64 = 0x6969;
        const SMB: i64 = 0x517B;
        const CIFS: i64 = 0xFF53_4D42;
        const SMB2: i64 = 0xFE53_4D42;
        const AFS: i64 = 0x5346_414F;
        const CEPH: i64 = 0x00C3_6400;
        matches!(st.f_type as i64, NFS | SMB | CIFS | SMB2 | AFS | CEPH)
    }
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        // `\\server\share` is remote; `\\?\C:\` and `\\.\` are local device paths.
        text.starts_with(r"\\") && !text.starts_with(r"\\?\") && !text.starts_with(r"\\.\")
            || text.to_ascii_lowercase().contains("\\onedrive")
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        false
    }
}

#[cfg(unix)]
fn nearest_existing(path: &Path) -> std::path::PathBuf {
    path.ancestors()
        .find(|p| !p.as_os_str().is_empty() && p.exists())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}
