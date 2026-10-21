//! Extended ACLs under the owner-only guarantee (Unix).
//!
//! `0600` and `0700` constrain the BSD mode bits only. macOS evaluates a file's ACL before those
//! bits, and a new file or directory inherits the *inheritable* entries of its parent's ACL when
//! it is created. A parent that carries `everyone allow read,file_inherit` therefore makes every
//! `0600` file created below it readable by everyone, whatever the mode says.
//!
//! This module only **reads** ACLs and **refuses**. It never changes one: removing an ACL from
//! an inode cannot revoke access through a descriptor somebody already holds open, so a "repair"
//! of an entry that was ever exposed would report success while the exposure stood.
//!
//! **One predicate.** An ACL is unsafe for the owner-only guarantee exactly when it has at least
//! one `allow` entry. A `deny` entry cannot widen access, so an ACL made only of `deny` entries
//! passes everywhere below. An entry whose type cannot be read counts as `allow` (fail closed),
//! and so does any type other than `deny`. The guarantee, on macOS (it is a no-op elsewhere, see
//! below):
//!
//! 1. **Before creating** a private file or directory, the parent's ACL is read. If it holds an
//!    inheritable (`file_inherit` or `directory_inherit`) `allow` entry the creation is refused
//!    with [`FsError::UnsafeAcl`] and nothing is created, so the new inode never exists with an
//!    `allow` entry it did not ask for. A non-inheritable `allow` entry, and any `deny` entry,
//!    inheritable or not, pass.
//! 2. **After creating**, the new inode's own ACL is read through its descriptor. If it has any
//!    `allow` entry (the parent's ACL changed between step 1 and the creation), the creation is
//!    refused with [`FsError::UnsafeAcl`]. Temporary creations remove the still-empty inode.
//!    [`crate::StateDir::create_new_empty`] retains its visible final name so an existing lock
//!    cannot be split across two inodes. Such a failed entry is not verified private. Nothing
//!    is filled into an inode that inherited access. Inherited `deny` entries are left on it.
//! 3. **Existing entries** ([`crate::restrict_owner_only`], [`crate::private::is_owner_only`])
//!    are inspected the same way: `restrict_owner_only` narrows the mode bits and then refuses an
//!    entry that has an `allow` entry ([`FsError::UnsafeAcl`]); it does not remove it. A
//!    deny-only entry succeeds, and `is_owner_only` is true for it when the mode bits allow.
//!
//! Linux and the other Unixes: this module does nothing. POSIX default ACLs on Linux are applied
//! to a new file masked by the creation mode, and a `0600` creation sets the mask to `---`, so
//! named-user and named-group entries grant nothing. That is the documented kernel behaviour and
//! is not checked here. Other ACL models (FreeBSD NFSv4 ACLs) are not handled.
//!
//! Every ACL call below is declared by hand against `<sys/acl.h>` on macOS and none of it has been
//! compiled or run in this tree's verification. The declarations are, exactly: `acl_get_fd_np`,
//! `acl_get_link_np`, `acl_free`, `acl_get_entry`, `acl_get_tag_type`, `acl_get_flagset_np`,
//! `acl_get_flag_np`. All seven only read; no call that sets or frees-and-replaces an ACL is
//! declared. `acl_get_link_np` reads the ACL of an EXISTING entry by path: such an entry is never
//! opened by this crate to inspect it, because closing a descriptor releases every POSIX record
//! lock the process holds on that file (SQLite's on a database and its `-wal`/`-shm`, this
//! crate's own on a lock file).

use crate::error::{FsError, Result};
use std::fs::File;
use std::path::Path;

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::{c_int, c_uint, c_void};
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;

    type Acl = *mut c_void;
    type Entry = *mut c_void;
    type Flagset = *mut c_void;

    /// `ACL_TYPE_EXTENDED`: the only ACL type macOS has.
    const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const ACL_NEXT_ENTRY: c_int = -1;
    /// `ACL_EXTENDED_DENY`. (`ACL_EXTENDED_ALLOW` is 1; every tag other than this one counts as
    /// an `allow` entry, so only this value is named.)
    const ACL_EXTENDED_DENY: c_int = 2;
    /// `ACL_ENTRY_FILE_INHERIT` and `ACL_ENTRY_DIRECTORY_INHERIT`.
    const ACL_ENTRY_FILE_INHERIT: c_uint = 1 << 5;
    const ACL_ENTRY_DIRECTORY_INHERIT: c_uint = 1 << 6;

    extern "C" {
        fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> Acl;
        /// By path, not following a link at the final component. Used for an entry that
        /// already exists, which must never be opened here (see `path_has_allow_entry`).
        fn acl_get_link_np(path: *const std::ffi::c_char, acl_type: c_int) -> Acl;
        fn acl_free(object: *mut c_void) -> c_int;
        fn acl_get_entry(acl: Acl, entry_id: c_int, entry: *mut Entry) -> c_int;
        fn acl_get_tag_type(entry: Entry, tag: *mut c_int) -> c_int;
        fn acl_get_flagset_np(object: *mut c_void, flagset: *mut Flagset) -> c_int;
        fn acl_get_flag_np(flagset: Flagset, flag: c_uint) -> c_int;
    }

    /// The ACL of `file`, or `None` when it has none or its filesystem cannot hold one.
    fn read(file: &File) -> io::Result<Option<Acl>> {
        // SAFETY: the descriptor is open for the call; the result is owned by the caller.
        let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
        if !acl.is_null() {
            return Ok(Some(acl));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOENT) | Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP) => Ok(None),
            _ => Err(error),
        }
    }

    /// Calls `visit` with each entry of `acl` in turn; true as soon as one call returns true.
    fn any_entry(acl: Acl, mut visit: impl FnMut(Entry) -> bool) -> bool {
        let mut which = ACL_FIRST_ENTRY;
        loop {
            let mut entry: Entry = std::ptr::null_mut();
            // SAFETY: `acl` is a live ACL and `entry` a valid out-parameter. The call fails
            // (nonzero) when there is no further entry.
            if unsafe { acl_get_entry(acl, which, &mut entry) } != 0 {
                return false;
            }
            if visit(entry) {
                return true;
            }
            which = ACL_NEXT_ENTRY;
        }
    }

    /// True when `entry` is an `allow` entry: its type cannot be read (fail closed), or it is
    /// anything but `ACL_EXTENDED_DENY`.
    fn is_allow(entry: Entry) -> bool {
        let mut tag: c_int = 0;
        // SAFETY: `entry` belongs to a live ACL; `tag` is a valid out-parameter.
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
            return true;
        }
        tag != ACL_EXTENDED_DENY
    }

    /// True when `file` has an ACL with at least one `allow` entry. A deny-only ACL is false.
    pub fn has_allow_entry(file: &File) -> io::Result<bool> {
        let Some(acl) = read(file)? else {
            return Ok(false);
        };
        let any = any_entry(acl, is_allow);
        // SAFETY: `acl` came from `acl_get_fd_np` and is freed once.
        unsafe { acl_free(acl) };
        Ok(any)
    }

    /// [`has_allow_entry`] for an entry that already exists, read by path with no descriptor.
    ///
    /// An existing file must not be opened and closed here: closing ANY descriptor a process
    /// holds on a file releases every POSIX record lock that process holds on it. A caller
    /// hardening `<db>-shm` or `<db>-wal` would drop SQLite's own locks on them, another process
    /// could then rebuild the WAL index under a live mapping, and the next append dies with a
    /// bus error (seen by a consumer on 2026-10-12). The same holds for a lock file of this
    /// crate. `acl_get_link_np` opens nothing. A link at `path` is not followed. A path that
    /// no longer exists reads as "no ACL" (the call reports `ENOENT` for both).
    pub fn path_has_allow_entry(path: &std::path::Path) -> io::Result<bool> {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
        // SAFETY: `c_path` is a valid NUL-terminated string for the call; the result is owned
        // here and freed below.
        let acl = unsafe { acl_get_link_np(c_path.as_ptr(), ACL_TYPE_EXTENDED) };
        if acl.is_null() {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ENOENT) | Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP) => Ok(false),
                _ => Err(error),
            };
        }
        let any = any_entry(acl, is_allow);
        // SAFETY: `acl` came from `acl_get_link_np` and is freed once.
        unsafe { acl_free(acl) };
        Ok(any)
    }

    /// True when the ACL of directory `dir` has an `allow` entry that a new file or directory
    /// inherits. A `deny` entry never counts. An entry whose type or flags cannot be read counts
    /// as an inheritable `allow` (fail closed).
    pub fn dir_inherits_allow(dir: &File) -> io::Result<bool> {
        let Some(acl) = read(dir)? else {
            return Ok(false);
        };
        let found = any_entry(acl, |entry| {
            if !is_allow(entry) {
                return false;
            }
            let mut flags: Flagset = std::ptr::null_mut();
            // SAFETY: `entry` belongs to the live `acl`; `flags` is a valid out-parameter.
            if unsafe { acl_get_flagset_np(entry, &mut flags) } != 0 {
                return true;
            }
            // SAFETY: `flags` was just returned for a live entry.
            let (file, directory) = unsafe {
                (
                    acl_get_flag_np(flags, ACL_ENTRY_FILE_INHERIT),
                    acl_get_flag_np(flags, ACL_ENTRY_DIRECTORY_INHERIT),
                )
            };
            file != 0 || directory != 0
        });
        // SAFETY: `acl` came from `acl_get_fd_np` and is freed once.
        unsafe { acl_free(acl) };
        Ok(found)
    }
}

#[cfg(not(target_os = "macos"))]
mod sys {
    use std::fs::File;
    use std::io;

    pub fn has_allow_entry(_file: &File) -> io::Result<bool> {
        Ok(false)
    }
    pub fn path_has_allow_entry(_path: &std::path::Path) -> io::Result<bool> {
        Ok(false)
    }
    pub fn dir_inherits_allow(_dir: &File) -> io::Result<bool> {
        Ok(false)
    }
}

/// Step 1: refuse to create a private entry in the directory open as `parent` when it would
/// inherit an `allow` ACL entry. `shown` names the entry being created.
pub(crate) fn guard_parent(parent: &File, shown: &Path) -> Result<()> {
    match sys::dir_inherits_allow(parent) {
        Ok(false) => Ok(()),
        Ok(true) => Err(FsError::UnsafeAcl(shown.to_path_buf())),
        Err(e) => Err(FsError::io(shown, e)),
    }
}

/// [`guard_parent`] for a parent named by path. A parent that does not exist passes: the
/// creation that follows reports that failure itself.
pub(crate) fn guard_parent_path(parent: &Path, shown: &Path) -> Result<()> {
    if cfg!(not(target_os = "macos")) {
        return Ok(());
    }
    match open_directory(parent) {
        Ok(dir) => guard_parent(&dir, shown),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // Not a directory: nothing can be created under it, and the creation says so itself.
        // It was not opened (see `open_directory`).
        Err(e) if is_not_a_directory(&e) => Ok(()),
        Err(e) => Err(FsError::io(parent, e)),
    }
}

/// Opens `path` only if it is a directory, decided by the kernel in the open itself
/// (`O_DIRECTORY`), never by a look beforehand.
///
/// A caller can hand this module a path it believes is a directory and that is, or has
/// become, a regular file: `create_private_new(p.join("child"))` where `p` is a file, say.
/// Opening and closing a regular file releases every POSIX record lock this process holds on
/// it (its own `FileLock`, SQLite's locks on a database), so a failed creation would silently
/// end an exclusion somewhere else. With `O_DIRECTORY` such a path is refused without being
/// opened. A directory carries no record lock of this crate or of SQLite.
fn open_directory(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        File::open(path)
    }
}

fn is_not_a_directory(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ENOTDIR)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

/// Step 2 (and step 3): `Ok` when `file` carries no `allow` ACL entry (no ACL at all, or only
/// `deny` entries), [`FsError::UnsafeAcl`] when it carries one. Nothing is changed. For an entry
/// just created temporarily the caller removes the still-empty entry on `Err`; an existing
/// entry or a visible permanent empty-file name is left as it is.
pub(crate) fn refuse_acl(file: &File, shown: &Path) -> Result<()> {
    match sys::has_allow_entry(file) {
        Ok(false) => Ok(()),
        Ok(true) => Err(FsError::UnsafeAcl(shown.to_path_buf())),
        Err(e) => Err(FsError::io(shown, e)),
    }
}

/// [`refuse_acl`] for a directory just created at `path` (off macOS: nothing to do).
pub(crate) fn refuse_acl_path(path: &Path) -> Result<()> {
    if cfg!(not(target_os = "macos")) {
        return Ok(());
    }
    // `O_DIRECTORY`: were the new directory swapped for a file, that file is not opened.
    let dir = open_directory(path).map_err(|e| FsError::io(path, e))?;
    refuse_acl(&dir, path)
}

/// Whether the existing entry at `path` carries an `allow` ACL entry (always false off macOS).
/// Read by path: the entry is never opened, so no lock this process holds on it is released.
pub(crate) fn existing_has_allow_acl(path: &Path) -> Result<bool> {
    sys::path_has_allow_entry(path).map_err(|e| FsError::io(path, e))
}
