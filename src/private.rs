//! Owner-only files and directories.
//!
//! Donor: CodeRight `private_fs` (perm/atomic/sweep). Unix uses mode bits applied at
//! creation time (never chmod-after-create for new files); Windows applies one protected
//! owner-only DACL through native security APIs. There is no weaker fallback: a failed native
//! step is an error, never a partial hardening reported as success.

use crate::atomic::{atomic_write_with, AtomicOptions};
use crate::error::{FsError, Result};
use std::path::{Path, PathBuf};

pub const FILE_MODE_OWNER_ONLY: u32 = 0o600;
pub const DIR_MODE_OWNER_ONLY: u32 = 0o700;
/// Mode of an owner-only executable file on Unix (owner read, write and execute).
pub const EXECUTABLE_MODE_OWNER_ONLY: u32 = 0o700;

/// Narrow an existing file or directory to owner-only. A missing path is a no-op.
/// Symlinks are never followed on unix: a symlink is refused rather than chmod-ing its target.
/// On macOS an entry that carries an extended ACL is **refused** ([`FsError::UnsafeAcl`]) after its
/// mode has been narrowed: the ACL is left as it is, because removing it cannot revoke access
/// through a descriptor somebody already holds open, so a repair would report an owner-only
/// entry that was not.
pub fn restrict_owner_only(path: &Path) -> Result<()> {
    restrict_owner_only_impl(path)
}

#[cfg(unix)]
fn restrict_owner_only_impl(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(FsError::io(path, e)),
    };
    if meta.file_type().is_symlink() {
        return Err(FsError::Symlink(path.to_path_buf()));
    }
    let mode = if meta.is_dir() {
        DIR_MODE_OWNER_ONLY
    } else {
        FILE_MODE_OWNER_ONLY
    };
    if meta.permissions().mode() & 0o7777 != mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| FsError::io(path, e))?;
    }
    // The mode bits are only half of the guarantee: on macOS an ACL `allow` entry is evaluated
    // before them, so a `0600` file can still be readable by others. An ACL is not removed (that
    // cannot revoke an open descriptor); the entry is refused, so the caller knows it is not
    // owner-only.
    //
    // The ACL is read by path. The entry is NOT opened: callers apply this to live files
    // (rightkit-db hardens `<db>-wal` and `<db>-shm` on every open), and closing a descriptor
    // releases every POSIX record lock this process holds on the file, SQLite's included.
    if cfg!(target_os = "macos")
        && (meta.is_file() || meta.is_dir())
        && crate::acl::existing_has_allow_acl(path)?
    {
        return Err(FsError::UnsafeAcl(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn restrict_owner_only_impl(_path: &Path) -> Result<()> {
    Ok(())
}

/// True when no group/other access exists on unix: no group or other mode bits and, on macOS, no
/// extended ACL (an `allow` entry is evaluated before the mode bits). On macOS an entry that
/// cannot be opened to read its ACL is reported as not owner-only, because that cannot be
/// proven.
///
/// On Windows this is a **verify-only trust check**: it reads the entry's owner and DACL through a
/// no-reparse handle and is true when the owner is this process's `TokenUser` (or an enabled
/// owner-capable group of its token, in practice Administrators in an elevated token) **and** the DACL
/// grants access only to principals that cannot be a foreign party:
///
/// - the current user,
/// - `NT AUTHORITY\SYSTEM` (`S-1-5-18`),
/// - `BUILTIN\Administrators` (`S-1-5-32-544`),
/// - `CREATOR OWNER` (`S-1-3-0`) when the entry is inherit-only (it grants nothing on this object and
///   is replaced by the creator in what inherits it).
///
/// The ACEs may be explicit or **inherited**, and the DACL need not be protected, so a file or
/// directory made below `%LOCALAPPDATA%` (whose ACL is `SYSTEM`, `Administrators` and the user,
/// inherited) is owner-only without the crate having rewritten its ACL (rightkit#676, rightkit#517).
/// Any `allow` ACE for another user, `Everyone`, `Users`, `Authenticated Users` or any other
/// principal makes the answer `false`, as do a missing (NULL) DACL, an unreadable ACE and any ACE type
/// this check does not understand (it fails closed). `deny` ACEs cannot widen access and are ignored.
/// Unsupported ACL filesystems and unreadable security metadata are errors.
///
/// This is a read of the state now, not a guarantee for later: whoever may change the DACL can change
/// the answer. The exact form this crate itself writes (one protected ACE for the user) is checked by
/// [`is_owner_only_strict`]; the objects this crate creates are verified against that stricter form
/// before any byte is written.
pub fn is_owner_only(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::symlink_metadata(path).map_err(|e| FsError::io(path, e))?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Ok(false);
        }
        if cfg!(target_os = "macos") && (meta.is_file() || meta.is_dir()) {
            // By path, never by opening: see `restrict_owner_only`.
            return Ok(!crate::acl::existing_has_allow_acl(path)?);
        }
        Ok(true)
    }
    #[cfg(windows)]
    {
        let file = open_owner_only_handle(path, false)?;
        Ok(private_trust_handle(&file, path, true)? == Trust::Private)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(true)
    }
}

/// The exact owner-only form this crate writes. Unix: the same as [`is_owner_only`]. Windows: true
/// only for an object owned by this process's `TokenUser` (or an enabled owner-capable token group)
/// whose DACL is **protected** and holds exactly one ACE, a full-control `allow` for the current user
/// (inheritable on a directory). Unlike [`is_owner_only`] it rejects an inherited ACL and an extra
/// `SYSTEM` or `Administrators` ACE, so it is `false` for a file this crate did not harden.
pub fn is_owner_only_strict(path: &Path) -> Result<bool> {
    #[cfg(windows)]
    {
        let file = open_owner_only_handle(path, false)?;
        is_owner_only_handle(&file, path)
    }
    #[cfg(not(windows))]
    {
        is_owner_only(path)
    }
}

/// [`is_owner_only`] as a checked call: `Ok(())` when `path` is owner-only and owned by the current
/// user, else the reason. For a caller that must refuse rather than branch (a trust check on a file
/// or directory it did not make, such as a runtime store below `%LOCALAPPDATA%`).
///
/// - [`FsError::NotOwned`]: owned by somebody else (Unix: `st_uid` is not the effective user;
///   Windows: the owner is neither the token user nor an enabled owner-capable group).
/// - [`FsError::TooPermissive`]: another principal can reach it (Unix mode bits or, on macOS, an
///   ACL `allow` entry; Windows: an `allow` ACE as described on [`is_owner_only`]).
/// - [`FsError::Symlink`]: `path` is a symlink or reparse point (never followed).
///
/// Nothing is changed or repaired; [`restrict_owner_only`] is the repair.
pub fn verify_owner_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path).map_err(|e| FsError::io(path, e))?;
        if meta.file_type().is_symlink() {
            return Err(FsError::Symlink(path.to_path_buf()));
        }
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(FsError::NotOwned(path.to_path_buf()));
        }
        if is_owner_only(path)? {
            Ok(())
        } else {
            Err(FsError::TooPermissive(path.to_path_buf()))
        }
    }
    #[cfg(windows)]
    {
        let file = open_owner_only_handle(path, false)?;
        match private_trust_handle(&file, path, true)? {
            Trust::Private => Ok(()),
            Trust::ForeignOwner => Err(FsError::NotOwned(path.to_path_buf())),
            Trust::ForeignAccess => Err(FsError::TooPermissive(path.to_path_buf())),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

/// What a DACL inspection concluded (Windows).
#[cfg(any(windows, test))]
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trust {
    Private,
    ForeignOwner,
    ForeignAccess,
}

/// What an ACE does, from its type byte: `Allow` (`ACCESS_ALLOWED` and the callback form, whose
/// trustee SID sits right after the access mask), `Deny` (the four deny types, which cannot widen
/// access) or `Other` (object, compound and every unknown type: not understood, so not trusted).
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AceKind {
    Allow,
    Deny,
    Other,
}

#[cfg(any(windows, test))]
fn ace_kind(ace_type: u8) -> AceKind {
    match ace_type {
        0 | 9 => AceKind::Allow,
        1 | 6 | 10 | 12 => AceKind::Deny,
        _ => AceKind::Other,
    }
}

/// `INHERIT_ONLY_ACE`: the ACE grants nothing on this object, only on what inherits it.
#[cfg(any(windows, test))]
const INHERIT_ONLY_ACE: u8 = 0x08;

/// Whether an `allow` ACE for this trustee leaves the object owner-only: the current user, SYSTEM
/// (`S-1-5-18`), BUILTIN\Administrators (`S-1-5-32-544`), or CREATOR OWNER (`S-1-3-0`) on an
/// inherit-only ACE. `authority` is the 48-bit identifier authority.
#[cfg(any(windows, test))]
fn trustee_is_acceptable(
    is_current_user: bool,
    authority: u64,
    sub_authorities: &[u32],
    ace_flags: u8,
) -> bool {
    if is_current_user {
        return true;
    }
    match (authority, sub_authorities) {
        (5, [18]) | (5, [32, 544]) => true,
        (3, [0]) => ace_flags & INHERIT_ONLY_ACE != 0,
        _ => false,
    }
}

/// Whether one ACE leaves the object owner-only: a `deny` ACE always does (it cannot widen access);
/// an `allow` ACE does when its trustee is acceptable ([`trustee_is_acceptable`]); an ACE of any
/// other type, or an `allow` ACE whose trustee could not be read (`None`), does not (fail closed).
/// `trustee` is `(is_current_user, authority, sub_authorities)`.
#[cfg(any(windows, test))]
fn ace_is_acceptable(ace_type: u8, ace_flags: u8, trustee: Option<(bool, u64, &[u32])>) -> bool {
    match ace_kind(ace_type) {
        AceKind::Deny => true,
        AceKind::Other => false,
        AceKind::Allow => match trustee {
            Some((is_user, authority, subs)) => {
                trustee_is_acceptable(is_user, authority, subs, ace_flags)
            }
            None => false,
        },
    }
}

/// Create `dir` and any missing ancestors. Each *newly created* directory is owner-only
/// before a child is created inside it; pre-existing ancestors are left alone, and the leaf
/// is always hardened. A symlinked leaf is refused. This does not sync directories: it is not a
/// durable operation (the durable form is [`crate::atomic::create_dir_all_durable`]). On macOS a
/// directory is not created under a parent whose ACL would give it an inheritable `allow` entry, and
/// one that got an `allow` entry anyway is removed again ([`FsError::UnsafeAcl`]).
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    if dir.as_os_str().is_empty() {
        return Err(FsError::io(
            dir,
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty path"),
        ));
    }
    let mut missing: Vec<PathBuf> = Vec::new();
    for ancestor in dir.ancestors() {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        match ancestor.symlink_metadata() {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.to_path_buf());
            }
            Err(error) => return Err(FsError::io(ancestor, error)),
        }
    }
    for path in missing.iter().rev() {
        create_private_dir_one(path)?;
    }
    let meta = std::fs::symlink_metadata(dir).map_err(|e| FsError::io(dir, e))?;
    if meta.file_type().is_symlink() {
        return Err(FsError::Symlink(dir.to_path_buf()));
    }
    if !meta.is_dir() {
        return Err(FsError::NotADirectory(dir.to_path_buf()));
    }
    restrict_owner_only(dir)
}

fn create_private_dir_one(path: &Path) -> Result<()> {
    #[cfg(unix)]
    let result = {
        use std::os::unix::fs::DirBuilderExt;
        // macOS: refuse a parent whose ACL would give the new directory an inheritable `allow` entry.
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            crate::acl::guard_parent_path(parent, path)?;
        }
        let made = std::fs::DirBuilder::new()
            .mode(DIR_MODE_OWNER_ONLY)
            .create(path);
        if made.is_ok() {
            // A directory that carries an `allow` entry anyway is empty: remove it and refuse.
            if let Err(error) = crate::acl::refuse_acl_path(path) {
                let _ = std::fs::remove_dir(path);
                return Err(error);
            }
        }
        made
    };
    #[cfg(windows)]
    let result = create_dir_owner_only(path);
    #[cfg(not(any(unix, windows)))]
    let result = std::fs::create_dir(path);
    let created = match result {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(FsError::io(path, e)),
    };
    #[cfg(not(windows))]
    let _ = created;
    #[cfg(windows)]
    if !created {
        restrict_owner_only(path)?;
    }
    #[cfg(not(any(unix, windows)))]
    restrict_owner_only(path)?;
    Ok(())
}

/// Atomically write an owner-only file (the temp inode is owner-only from creation, so the
/// secret is never visible under the destination name with a permissive mode).
pub fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    atomic_write_with(path, AtomicOptions::private(), |f| f.write_all(bytes))
}

pub fn write_private_text(path: &Path, text: &str) -> Result<()> {
    write_private_file(path, text.as_bytes())
}

/// Atomically write an owner-only executable file (M8-O10: helper and askpass scripts).
///
/// Unix: the published file is mode `0700` ([`EXECUTABLE_MODE_OWNER_ONLY`]). The temp inode is
/// created `0600` and set to `0700` through its descriptor before it is published, so the file
/// never exists under the destination name with a broader mode, and never without its execute
/// bit. Windows: there is no execute bit; the file gets the same owner-only DACL as
/// [`write_private_file`] and is executable by file type, so the caller picks the extension.
/// Like every atomic write here the destination is replaced as a whole, and a name shaped like
/// a generated temp name is refused ([`FsError::BadName`]).
pub fn write_private_executable(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut writer = crate::atomic::AtomicWriter::create(path, AtomicOptions::private())?;
    writer.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(EXECUTABLE_MODE_OWNER_ONLY);
        if let Err(e) = writer.file_mut().set_permissions(permissions) {
            let temp = writer.temp_path().to_path_buf();
            return Err(FsError::io(temp, e));
        }
    }
    writer.commit()
}

/// Exclusively create an owner-only file, never following a symlink at `path`.
/// For files that are appended to or locked rather than replaced (logs, lock files).
///
/// The file is owner-only from the instant it exists: Unix creates it `0600`, Windows creates it
/// with an owner-only security descriptor passed to `CreateFileW`, so no principal the parent's
/// inherited ACL admits can open it before it is restricted and keep a readable handle. A name
/// shaped like a temp name this crate generates is refused ([`FsError::BadName`]); a sweep would
/// delete such a file.
pub fn create_private_new(path: &Path) -> Result<std::fs::File> {
    if let Some(name) = path.file_name() {
        crate::atomic::reject_reserved_name(&name.to_string_lossy())?;
    }
    create_private_new_impl(path)
}

#[cfg(windows)]
fn create_private_new_impl(path: &Path) -> Result<std::fs::File> {
    create_new_owner_only(path).map_err(|e| FsError::io(path, e))
}

#[cfg(not(windows))]
fn create_private_new_impl(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(FILE_MODE_OWNER_ONLY)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    // macOS: refuse a parent whose ACL would give the new file an inheritable `allow` entry, before
    // creating.
    #[cfg(unix)]
    crate::acl::guard_parent_path(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
        path,
    )?;
    let file = options.open(path).map_err(|e| FsError::io(path, e))?;
    #[cfg(unix)]
    // A file that carries an `allow` entry anyway is still empty: remove it and refuse.
    if let Err(error) = crate::acl::refuse_acl(&file, path) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    #[cfg(not(unix))]
    restrict_owner_only(path)?;
    Ok(file)
}

/// Owner-only repair for the named entries directly inside `dir`, plus `dir` itself.
/// One level deep and by exact name only: foreign entries are untouched and no symlink is
/// followed. Returns how many entries were narrowed.
pub fn sweep_owner_only(dir: &Path, owned_names: &[&str]) -> Result<usize> {
    restrict_owner_only(dir)?;
    let mut narrowed = 0;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(FsError::io(dir, e)),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if owned_names.iter().any(|owned| name == *owned)
            && (cfg!(windows) || !is_owner_only(&entry.path())?)
        {
            restrict_owner_only(&entry.path())?;
            narrowed += 1;
        }
    }
    Ok(narrowed)
}

#[cfg(windows)]
fn restrict_owner_only_impl(path: &Path) -> crate::error::Result<()> {
    let file = match open_owner_only_handle(path, true) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return Ok(()),
        Err(error) => return Err(error),
    };
    // Fail closed. The native call replaces the whole DACL (one protected ACE for the current
    // user, nothing inherited). An `icacls /inheritance:r /grant:r` fallback is not equivalent: it
    // only replaces the grants for the one SID, so an explicit ACE for another principal (an
    // Everyone read ACE on the destination, say) would survive and the result would be reported
    // as owner-only.
    native_restrict_owner_only(&file, path)
}

#[cfg(windows)]
fn current_process_token_user(path: &Path) -> crate::error::Result<Vec<usize>> {
    current_process_token_info(path, 1) // TokenUser
}

/// Raw `GetTokenInformation` buffer for the current process token (pointer-aligned storage).
#[cfg(windows)]
fn current_process_token_info(path: &Path, class: u32) -> crate::error::Result<Vec<usize>> {
    use std::ffi::c_void;

    type Handle = *mut c_void;
    const TOKEN_QUERY: u32 = 0x0008;
    let class_id = class;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(
            token: Handle,
            class: u32,
            buffer: *mut c_void,
            len: u32,
            needed: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
    }

    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(crate::error::FsError::io(
                path,
                std::io::Error::last_os_error(),
            ));
        }
        let mut needed = 0;
        let _ = GetTokenInformation(token, class_id, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            let error = std::io::Error::last_os_error();
            let _ = CloseHandle(token);
            return Err(crate::error::FsError::io(path, error));
        }
        let words = (needed as usize).div_ceil(std::mem::size_of::<usize>());
        let mut token_info = vec![0usize; words];
        if GetTokenInformation(
            token,
            class_id,
            token_info.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) == 0
        {
            let error = std::io::Error::last_os_error();
            let _ = CloseHandle(token);
            return Err(crate::error::FsError::io(path, error));
        }
        let _ = CloseHandle(token);
        Ok(token_info)
    }
}

/// `SE_GROUP_ENABLED`: the group is active for access checks in this token. An elevated
/// administrator holds BUILTIN\\Administrators enabled; a filtered (non-elevated) token holds it
/// deny-only, which is not enabled.
#[cfg(any(windows, test))]
const SE_GROUP_ENABLED: u32 = 0x0000_0004;
#[cfg(any(windows, test))]
const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x0000_0010;

/// `SE_GROUP_OWNER`: the group may be assigned as the owner of objects the token creates.
#[cfg(any(windows, test))]
const SE_GROUP_OWNER: u32 = 0x0000_0008;

/// True for the well-known broad SIDs that must never be accepted as an owner: Everyone
/// (S-1-1-0), LOCAL (S-1-2-0), INTERACTIVE (S-1-5-4), Authenticated Users (S-1-5-11) and
/// BUILTIN\Users (S-1-5-32-545). `authority` is the 48-bit identifier authority.
#[cfg(any(windows, test))]
fn sid_is_broad(authority: u64, sub_authorities: &[u32]) -> bool {
    matches!(
        (authority, sub_authorities),
        (1, [0]) | (2, [0]) | (5, [4]) | (5, [11]) | (5, [32, 545])
    )
}

/// Decides whether an existing object's owner may be repaired to owner-only. Accepts only the
/// token user, or a group that is flagged `SE_GROUP_OWNER` and `SE_GROUP_ENABLED` and is not
/// deny-only (in practice BUILTIN\Administrators in an elevated token, which is where objects
/// created by an elevated process are owned). An owner implicitly holds WRITE_DAC, so a broad
/// owner would leave the object writable by others after repair; an ordinary enabled group
/// (Everyone, Users, Authenticated Users, INTERACTIVE, LOCAL, ...) is therefore never accepted,
/// and `is_broad` hard-denies those well-known SIDs even if a token flags them as owner groups.
#[cfg(any(windows, test))]
fn owner_is_trusted<S: PartialEq>(
    owner: &S,
    user: &S,
    groups: &[(S, u32)],
    is_broad: &dyn Fn(&S) -> bool,
) -> bool {
    if owner == user {
        return true;
    }
    if is_broad(owner) {
        return false;
    }
    groups.iter().any(|(sid, attributes)| {
        sid == owner
            && attributes & SE_GROUP_ENABLED != 0
            && attributes & SE_GROUP_OWNER != 0
            && attributes & SE_GROUP_USE_FOR_DENY_ONLY == 0
    })
}

/// True when `owner` is the current user or an enabled owner-capable, non-broad group of the token.
#[cfg(windows)]
fn owner_matches_token(
    owner: *mut std::ffi::c_void,
    user: *mut std::ffi::c_void,
    path: &Path,
) -> crate::error::Result<bool> {
    use std::ffi::c_void;
    #[repr(C)]
    struct SidAndAttributes {
        sid: *mut c_void,
        attributes: u32,
    }
    #[repr(C)]
    struct TokenGroups {
        count: u32,
        groups: [SidAndAttributes; 1],
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn EqualSid(left: *mut c_void, right: *mut c_void) -> i32;
    }
    const TOKEN_GROUPS_CLASS: u32 = 2;
    if owner.is_null() {
        return Ok(false);
    }
    // SAFETY: both SIDs are live and validated by the callers' queries.
    if unsafe { EqualSid(owner, user) } != 0 {
        return Ok(true);
    }
    let buffer = current_process_token_info(path, TOKEN_GROUPS_CLASS)?;
    let header = buffer.as_ptr().cast::<TokenGroups>();
    // SAFETY: a TOKEN_GROUPS buffer starts with its count followed by that many entries, all
    // inside the retained allocation.
    let (count, first) = unsafe { ((*header).count as usize, (*header).groups.as_ptr()) };
    // Reduce each group to "is it the owner" (1) or not (2) so the shared rule decides.
    let groups: Vec<(u8, u32)> = (0..count)
        .map(|index| {
            // SAFETY: index < GroupCount; group SIDs live in `buffer`, owner is live.
            let entry = unsafe { &*first.add(index) };
            let same = unsafe { EqualSid(owner, entry.sid) } != 0;
            (if same { 1 } else { 2 }, entry.attributes)
        })
        .collect();
    // SAFETY: the owner SID is live and valid (checked non-null above); a SID is revision,
    // sub-authority count (<= 15), 6 big-endian authority bytes, then that many u32 values.
    let owner_broad = unsafe {
        let bytes = owner.cast::<u8>();
        let count = (*bytes.add(1)).min(15) as usize;
        let mut authority = 0u64;
        for i in 0..6 {
            authority = (authority << 8) | u64::from(*bytes.add(2 + i));
        }
        let subs: Vec<u32> = (0..count)
            .map(|i| std::ptr::read_unaligned(bytes.add(8 + 4 * i).cast::<u32>()))
            .collect();
        sid_is_broad(authority, &subs)
    };
    Ok(owner_is_trusted(&1u8, &0u8, &groups, &|_| owner_broad))
}

#[cfg(windows)]
pub(crate) fn current_process_sid_string(path: &Path) -> crate::error::Result<String> {
    use std::ffi::c_void;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ConvertSidToStringSidW(sid: *mut c_void, string_sid: *mut *mut u16) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    let token_info = current_process_token_user(path)?;
    unsafe {
        let sid = *(token_info.as_ptr().cast::<*mut c_void>());
        let mut sid_text = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut sid_text) == 0 {
            return Err(crate::error::FsError::io(
                path,
                std::io::Error::last_os_error(),
            ));
        }
        let sid_len = (0..).take_while(|&i| *sid_text.add(i) != 0).count();
        let sid_string = String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, sid_len));
        let _ = LocalFree(sid_text.cast());
        Ok(sid_string)
    }
}

/// Read or repair only through this no-reparse handle. Repair asks for
/// `READ_CONTROL | WRITE_DAC | WRITE_OWNER` (so a trusted group owner can be rewritten to the
/// token user) and shares read, write and delete, so hardening a file another handle holds open
/// (SQLite) neither fails with a sharing violation nor releases that handle's locks. (An earlier
/// `MAXIMUM_ALLOWED` open added DELETE and broke that; its claimed inherited-ACE propagation
/// effect was never verified.) When `WRITE_OWNER` itself is refused, the open is retried without
/// it: an object already owned by the token user still repairs its DACL through the owner's
/// implicit `WRITE_DAC`, and one whose owner would have to change then fails closed in
/// `native_restrict_owner_only` instead of being reported repaired.
#[cfg(windows)]
fn open_owner_only_handle(path: &Path, repair: bool) -> crate::error::Result<std::fs::File> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    #[repr(C)]
    struct AttributeTagInfo {
        attributes: u32,
        tag: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const c_void,
            disposition: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
        fn GetFileInformationByHandleEx(
            file: *mut c_void,
            class: i32,
            info: *mut c_void,
            size: u32,
        ) -> i32;
    }

    // Repair rewrites the DACL and, for a trusted group owner, the owner, so it asks for
    // WRITE_OWNER as well; MAXIMUM_ALLOWED would add DELETE and conflict with a holder (SQLite)
    // that did not share delete. Share read, write and delete so this handle never blocks or is
    // blocked.
    const WRITE_DAC: u32 = 0x0004_0000;
    const WRITE_OWNER: u32 = 0x0008_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x7;
    const READ_CONTROL: u32 = 0x0002_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_TAG_INFO: i32 = 9;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if wide[..wide.len() - 1].contains(&0) {
        return Err(FsError::BadName(path.display().to_string()));
    }
    let read_access = READ_CONTROL | FILE_READ_ATTRIBUTES;
    let accesses: &[u32] = if repair {
        &[
            read_access | WRITE_DAC | WRITE_OWNER,
            read_access | WRITE_DAC,
        ]
    } else {
        &[read_access]
    };
    let mut raw = std::ptr::null_mut();
    for (index, access) in accesses.iter().enumerate() {
        // SAFETY: `wide` is NUL-terminated and no security attribute is retained by this open.
        raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                *access,
                FILE_SHARE_READ_WRITE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if raw as isize != -1 && !raw.is_null() {
            break;
        }
        let error = std::io::Error::last_os_error();
        // Only a refused WRITE_OWNER retries with the smaller request (see the doc comment).
        if !(index + 1 < accesses.len() && error.raw_os_error() == Some(ERROR_ACCESS_DENIED)) {
            return Err(FsError::io(path, error));
        }
        raw = std::ptr::null_mut();
    }
    if raw as isize == -1 || raw.is_null() {
        return Err(FsError::io(path, std::io::Error::last_os_error()));
    }
    // SAFETY: CreateFileW returned a fresh owned handle.
    let file = unsafe { std::fs::File::from_raw_handle(raw) };
    let mut info = AttributeTagInfo {
        attributes: 0,
        tag: 0,
    };
    // SAFETY: live handle and correctly sized output structure.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_ATTRIBUTE_TAG_INFO,
            (&mut info as *mut AttributeTagInfo).cast(),
            std::mem::size_of::<AttributeTagInfo>() as u32,
        )
    } == 0
    {
        return Err(FsError::io(path, std::io::Error::last_os_error()));
    }
    if info.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(FsError::Symlink(path.to_path_buf()));
    }
    Ok(file)
}

#[cfg(windows)]
fn require_persistent_acls(file: &std::fs::File, shown: &Path) -> crate::error::Result<()> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetVolumeInformationByHandleW(
            file: *mut std::ffi::c_void,
            volume_name: *mut u16,
            volume_name_size: u32,
            serial: *mut u32,
            max_component: *mut u32,
            flags: *mut u32,
            filesystem_name: *mut u16,
            filesystem_name_size: u32,
        ) -> i32;
    }
    const FILE_PERSISTENT_ACLS: u32 = 0x8;
    let mut flags = 0;
    // SAFETY: only the flags out-parameter is requested for a live file handle.
    if unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    } == 0
    {
        return Err(FsError::io(shown, std::io::Error::last_os_error()));
    }
    if flags & FILE_PERSISTENT_ACLS == 0 {
        return Err(FsError::Unavailable(
            "Windows filesystem lacks persistent ACLs",
        ));
    }
    Ok(())
}

/// The strict verdict: the exact protected single-ACE form this crate writes. False for a readable
/// but non-private descriptor. Query and metadata errors propagate.
#[cfg(windows)]
pub(crate) fn is_owner_only_handle(
    file: &std::fs::File,
    shown: &Path,
) -> crate::error::Result<bool> {
    private_trust_handle(file, shown, false).map(|trust| trust == Trust::Private)
}

/// Inspect the owner and DACL of an open handle. `tolerant` false is the strict form (protected DACL,
/// exactly one full-control ACE for the current user); `tolerant` true accepts inherited and
/// unprotected DACLs whose every `allow` ACE names the current user, SYSTEM, Administrators (or an
/// inherit-only CREATOR OWNER), see [`is_owner_only`].
#[cfg(windows)]
fn private_trust_handle(
    file: &std::fs::File,
    shown: &Path,
    tolerant: bool,
) -> crate::error::Result<Trust> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct AceHeader {
        ace_type: u8,
        ace_flags: u8,
        ace_size: u16,
    }
    #[repr(C)]
    struct AclSizeInformation {
        ace_count: u32,
        acl_bytes_in_use: u32,
        acl_bytes_free: u32,
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetSecurityInfo(
            object: *mut c_void,
            object_type: u32,
            information: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn EqualSid(left: *mut c_void, right: *mut c_void) -> i32;
        fn GetSecurityDescriptorControl(
            descriptor: *mut c_void,
            control: *mut u16,
            revision: *mut u32,
        ) -> i32;
        fn IsValidAcl(acl: *mut c_void) -> i32;
        fn GetAclInformation(
            acl: *mut c_void,
            information: *mut c_void,
            length: u32,
            class: u32,
        ) -> i32;
        fn GetAce(acl: *mut c_void, index: u32, ace: *mut *mut c_void) -> i32;
        fn IsValidSid(sid: *mut c_void) -> i32;
        fn GetLengthSid(sid: *mut c_void) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    const SE_FILE_OBJECT: u32 = 1;
    const OWNER_SECURITY_INFORMATION: u32 = 0x1;
    const DACL_SECURITY_INFORMATION: u32 = 0x4;
    const SE_DACL_PROTECTED: u16 = 0x1000;
    const ACL_SIZE_INFORMATION: u32 = 2;
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const OBJECT_INHERIT_ACE: u8 = 0x1;
    const CONTAINER_INHERIT_ACE: u8 = 0x2;
    const FILE_ALL_ACCESS: u32 = 0x001f_01ff;
    const ACCESS_ALLOWED_ACE_PREFIX: usize = 8;
    const SID_HEADER_SIZE: usize = 8;
    const SID_MAX_SUB_AUTHORITIES: u8 = 15;

    /// The strict form: exactly one ACE, a full-control `allow` for `sid`, with the inherit flags the
    /// object kind needs and nothing else.
    fn strict_single_ace(
        dacl: *mut c_void,
        count: u32,
        directory: bool,
        sid: *mut c_void,
        shown: &Path,
    ) -> crate::error::Result<bool> {
        if count != 1 {
            return Ok(false);
        }
        let mut ace = std::ptr::null_mut();
        // SAFETY: validated ACL has exactly one indexed ACE.
        if unsafe { GetAce(dacl, 0, &mut ace) } == 0 {
            return Err(FsError::io(shown, std::io::Error::last_os_error()));
        }
        if ace.is_null() {
            return Ok(false);
        }
        // SAFETY: GetAce returned an ACE from native-validated ACL storage.
        let header = unsafe { &*(ace.cast::<AceHeader>()) };
        if header.ace_type != ACCESS_ALLOWED_ACE_TYPE {
            return Ok(false);
        }
        let expected_flags = if directory {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        };
        // ACCESS_ALLOWED_ACE stores its access mask immediately after ACE_HEADER and SID at +8.
        let ace_size = usize::from(header.ace_size);
        if header.ace_flags != expected_flags
            || ace_size < ACCESS_ALLOWED_ACE_PREFIX + SID_HEADER_SIZE
        {
            return Ok(false);
        }
        // SAFETY: AceSize establishes both the mask and SID offsets within this ACE.
        let bytes = ace.cast::<u8>();
        let mask = unsafe { *(bytes.add(4).cast::<u32>()) };
        let ace_sid = unsafe { bytes.add(ACCESS_ALLOWED_ACE_PREFIX).cast::<c_void>() };
        // SAFETY: ACE size contains SID's fixed revision/count/authority header.
        let sub_authorities = unsafe { *bytes.add(ACCESS_ALLOWED_ACE_PREFIX + 1) };
        if sub_authorities > SID_MAX_SUB_AUTHORITIES {
            return Ok(false);
        }
        let declared_sid_size = SID_HEADER_SIZE + usize::from(sub_authorities) * 4;
        if declared_sid_size > ace_size - ACCESS_ALLOWED_ACE_PREFIX {
            return Ok(false);
        }
        // SAFETY: complete declared SID is now bounded inside this native-validated ACE.
        if unsafe { IsValidSid(ace_sid) } == 0 {
            return Ok(false);
        }
        // SAFETY: validated SID is bounded inside this ACE.
        let sid_length = unsafe { GetLengthSid(ace_sid) } as usize;
        if sid_length != declared_sid_size || ace_size != ACCESS_ALLOWED_ACE_PREFIX + sid_length {
            return Ok(false);
        }
        // SAFETY: both SID pointers remain valid for this final identity check.
        Ok(mask == FILE_ALL_ACCESS && unsafe { EqualSid(ace_sid, sid) } != 0)
    }

    /// The trustee of an `allow` ACE: `(is the current user, identifier authority, sub-authorities)`,
    /// or `None` when the ACE is too short or its SID is malformed.
    fn read_trustee(
        ace: *mut c_void,
        ace_size: usize,
        sid: *mut c_void,
    ) -> Option<(bool, u64, Vec<u32>)> {
        if ace_size < ACCESS_ALLOWED_ACE_PREFIX + SID_HEADER_SIZE {
            return None;
        }
        let bytes = ace.cast::<u8>();
        // SAFETY: the ACE is at least prefix + SID header long (checked above).
        let sub_authorities = unsafe { *bytes.add(ACCESS_ALLOWED_ACE_PREFIX + 1) };
        if sub_authorities > SID_MAX_SUB_AUTHORITIES {
            return None;
        }
        let declared = SID_HEADER_SIZE + usize::from(sub_authorities) * 4;
        if declared > ace_size - ACCESS_ALLOWED_ACE_PREFIX {
            return None;
        }
        // SAFETY: the complete declared SID lies inside this native-validated ACE.
        let ace_sid = unsafe { bytes.add(ACCESS_ALLOWED_ACE_PREFIX) }.cast::<c_void>();
        if unsafe { IsValidSid(ace_sid) } == 0 {
            return None;
        }
        let mut authority = 0u64;
        let mut subs = Vec::with_capacity(usize::from(sub_authorities));
        // SAFETY: authority bytes and each sub-authority lie inside the declared SID.
        unsafe {
            for i in 0..6 {
                authority =
                    (authority << 8) | u64::from(*bytes.add(ACCESS_ALLOWED_ACE_PREFIX + 2 + i));
            }
            for i in 0..usize::from(sub_authorities) {
                subs.push(std::ptr::read_unaligned(
                    bytes
                        .add(ACCESS_ALLOWED_ACE_PREFIX + SID_HEADER_SIZE + 4 * i)
                        .cast::<u32>(),
                ));
            }
        }
        // SAFETY: both SIDs are valid for this comparison.
        let is_user = unsafe { EqualSid(ace_sid, sid) } != 0;
        Some((is_user, authority, subs))
    }

    /// The tolerant form: every `allow` ACE must name an acceptable trustee (`ace_is_acceptable`).
    fn tolerant_dacl_verdict(
        dacl: *mut c_void,
        count: u32,
        sid: *mut c_void,
        shown: &Path,
    ) -> crate::error::Result<Trust> {
        for index in 0..count {
            let mut ace = std::ptr::null_mut();
            // SAFETY: `index` is below the count of a native-validated ACL.
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
                return Err(FsError::io(shown, std::io::Error::last_os_error()));
            }
            if ace.is_null() {
                return Ok(Trust::ForeignAccess);
            }
            // SAFETY: GetAce returned an ACE from native-validated ACL storage.
            let header = unsafe { &*(ace.cast::<AceHeader>()) };
            let trustee = if ace_kind(header.ace_type) == AceKind::Allow {
                read_trustee(ace, usize::from(header.ace_size), sid)
            } else {
                None
            };
            let view = trustee
                .as_ref()
                .map(|(user, authority, subs)| (*user, *authority, subs.as_slice()));
            if !ace_is_acceptable(header.ace_type, header.ace_flags, view) {
                return Ok(Trust::ForeignAccess);
            }
        }
        Ok(Trust::Private)
    }

    require_persistent_acls(file, shown)?;
    let token = current_process_token_user(shown)?;
    // SAFETY: TOKEN_USER starts with a SID pointer inside the retained allocation.
    let sid = unsafe { *(token.as_ptr().cast::<*mut c_void>()) };
    let mut owner = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: live handle; returned pointers live until LocalFree(descriptor).
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(FsError::io(
            shown,
            std::io::Error::from_raw_os_error(result as i32),
        ));
    }
    let check = (|| -> crate::error::Result<Trust> {
        if descriptor.is_null() || owner.is_null() {
            return Ok(Trust::ForeignOwner);
        }
        // A NULL DACL grants everyone full access.
        if dacl.is_null() {
            return Ok(Trust::ForeignAccess);
        }
        let directory = file
            .metadata()
            .map_err(|error| FsError::io(shown, error))?
            .is_dir();
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: descriptor came from GetSecurityInfo and remains live for this query.
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
            return Err(FsError::io(shown, std::io::Error::last_os_error()));
        }
        if !tolerant && control & SE_DACL_PROTECTED == 0 {
            return Ok(Trust::ForeignAccess);
        }
        // The owner is the current user, or an enabled token group (Administrators when
        // elevated). The strict form's single ACE below still has to name the current user.
        if !owner_matches_token(owner, sid, shown)? {
            return Ok(Trust::ForeignOwner);
        }
        // SAFETY: native validator rejects malformed ACL data before indexed access.
        if unsafe { IsValidAcl(dacl) } == 0 {
            return Ok(Trust::ForeignAccess);
        }
        let mut info = AclSizeInformation {
            ace_count: 0,
            acl_bytes_in_use: 0,
            acl_bytes_free: 0,
        };
        // SAFETY: valid ACL and correctly sized output structure.
        if unsafe {
            GetAclInformation(
                dacl,
                (&mut info as *mut AclSizeInformation).cast(),
                std::mem::size_of::<AclSizeInformation>() as u32,
                ACL_SIZE_INFORMATION,
            )
        } == 0
        {
            return Err(FsError::io(shown, std::io::Error::last_os_error()));
        }
        if tolerant {
            return tolerant_dacl_verdict(dacl, info.ace_count, sid, shown);
        }
        let private = strict_single_ace(dacl, info.ace_count, directory, sid, shown)?;
        Ok(if private {
            Trust::Private
        } else {
            Trust::ForeignAccess
        })
    })();
    // SAFETY: GetSecurityInfo allocated this descriptor on success, including all returned views.
    unsafe { LocalFree(descriptor) };
    check
}

/// Verify the current owner-only state of this exact handle. This never repairs permissions,
/// removes an entry, or reopens a displayed path. Later permission changes remain possible.
pub(crate) fn verify_owner_only_handle(
    file: &std::fs::File,
    shown: &Path,
) -> crate::error::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().map_err(|error| FsError::io(shown, error))?;
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(FsError::NotOwned(shown.to_path_buf()));
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(FsError::TooPermissive(shown.to_path_buf()));
        }
        crate::acl::refuse_acl(file, shown)
    }
    #[cfg(windows)]
    {
        if is_owner_only_handle(file, shown)? {
            Ok(())
        } else {
            Err(FsError::TooPermissive(shown.to_path_buf()))
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, shown);
        Err(FsError::Unavailable(
            "owner-only handle verification requires Unix or Windows",
        ))
    }
}

/// Whether the pinned object's owner is exactly `sid` (the reverify after an owner rewrite).
#[cfg(windows)]
fn handle_owner_is(
    file: &std::fs::File,
    sid: *mut std::ffi::c_void,
    path: &Path,
) -> crate::error::Result<bool> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetSecurityInfo(
            object: *mut c_void,
            object_type: u32,
            information: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn EqualSid(left: *mut c_void, right: *mut c_void) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    const SE_FILE_OBJECT: u32 = 1;
    const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;
    let mut owner = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: live handle; the owner view lives until LocalFree(descriptor).
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(FsError::io(
            path,
            std::io::Error::from_raw_os_error(result as i32),
        ));
    }
    // SAFETY: both SIDs are live for this comparison.
    let same = !owner.is_null() && unsafe { EqualSid(owner, sid) } != 0;
    // SAFETY: GetSecurityInfo allocated this descriptor on success.
    unsafe { LocalFree(descriptor) };
    Ok(same)
}

#[cfg(windows)]
fn native_restrict_owner_only(file: &std::fs::File, path: &Path) -> crate::error::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
    const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
    const SE_FILE_OBJECT: u32 = 1;
    const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetSecurityInfo(
            object: *mut c_void,
            object_type: u32,
            information: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn SetSecurityInfo(
            object: *mut c_void,
            object_type: u32,
            information: u32,
            owner: *mut c_void,
            group: *mut c_void,
            dacl: *mut c_void,
            sacl: *mut c_void,
        ) -> u32;
        fn GetSecurityDescriptorDacl(
            descriptor: *mut c_void,
            present: *mut i32,
            dacl: *mut *mut c_void,
            defaulted: *mut i32,
        ) -> i32;
        fn EqualSid(left: *mut c_void, right: *mut c_void) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    require_persistent_acls(file, path)?;
    // Determine inheritance from the pinned object rather than looking the path up again.
    let directory = file
        .metadata()
        .map_err(|error| FsError::io(path, error))?
        .is_dir();
    let token_info = current_process_token_user(path)?;
    unsafe {
        let sid = *(token_info.as_ptr().cast::<*mut c_void>());
        let mut owner = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        let owner_result = GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        );
        if owner_result != 0 {
            return Err(FsError::io(
                path,
                std::io::Error::from_raw_os_error(owner_result as i32),
            ));
        }
        // Accept the current user or an enabled token group: elevated processes create objects
        // owned by BUILTIN\\Administrators. A foreign owner is refused here, never taken over.
        // A trusted group owner is rewritten to the token user below (an unelevated process is
        // not the enabled-group owner, so leaving it would strand the repair); a refused
        // WRITE_OWNER is the typed I/O error of the SetSecurityInfo call, never a success.
        // SAFETY: both SIDs are live for this comparison (owner until LocalFree below).
        let is_token_user = !owner.is_null() && EqualSid(owner, sid) != 0;
        let owned = owner_matches_token(owner, sid, path);
        LocalFree(descriptor);
        let owned = owned?;
        if !owned {
            return Err(FsError::NotOwned(path.to_path_buf()));
        }
        let descriptor = SecurityDescriptor::owner_only(path, directory)?;
        let mut present = 0;
        let mut dacl = std::ptr::null_mut();
        let mut defaulted = 0;
        if GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted) == 0 {
            return Err(FsError::io(path, std::io::Error::last_os_error()));
        }
        if present == 0 || dacl.is_null() {
            return Err(FsError::Unavailable("owner-only descriptor lacks DACL"));
        }
        // Inheritable ACEs on a directory are propagated by SetSecurityInfo to existing children
        // that inherit; this call only guarantees the pinned object itself ends owner-only. The
        // owner member is set in the same call when it is not already the token user, so the
        // object never sits with a protected user DACL under a foreign-to-unelevated owner.
        let (information, new_owner) = if is_token_user {
            (
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
            )
        } else {
            (
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                sid,
            )
        };
        let result = SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            information,
            new_owner,
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        );
        if result == 0 {
            verify_owner_only_handle(file, path)?;
            if !is_token_user && !handle_owner_is(file, sid, path)? {
                return Err(FsError::NotOwned(path.to_path_buf()));
            }
            Ok(())
        } else {
            Err(crate::error::FsError::io(
                path,
                std::io::Error::from_raw_os_error(result as i32),
            ))
        }
    }
}

/// A security descriptor built from SDDL: explicit TokenUser owner plus one protected ACE giving
/// that user full control,
/// freed on drop. A directory's ACE also inherits to what is created inside it. Used to create
/// files and directories owner-only from the start, with no window in which a wider inherited ACL
/// applies.
#[cfg(windows)]
pub(crate) struct SecurityDescriptor(pub(crate) *mut std::ffi::c_void);

#[cfg(windows)]
impl SecurityDescriptor {
    pub(crate) fn owner_only(shown: &Path, directory: bool) -> crate::error::Result<Self> {
        use std::ffi::c_void;
        #[link(name = "advapi32")]
        unsafe extern "system" {
            fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
                string: *const u16,
                revision: u32,
                descriptor: *mut *mut c_void,
                size: *mut u32,
            ) -> i32;
        }
        let sid = current_process_sid_string(shown)?;
        let inherit = if directory { "OICI" } else { "" };
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;{inherit};FA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut raw = std::ptr::null_mut();
        // SAFETY: NUL-terminated SDDL; the API allocates the returned descriptor.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut raw,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(FsError::io(shown, std::io::Error::last_os_error()));
        }
        Ok(Self(raw))
    }
}

#[cfg(windows)]
impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        }
        // SAFETY: allocation returned by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

/// Exclusively creates `path` for writing with the owner-only security descriptor passed to
/// `CreateFileW`, so the file never exists with the parent's inherited ACL (no principal that ACL
/// admits can open it before it is restricted). Fails if the descriptor cannot be built, or if
/// `path` exists (a link at the name is never followed: `CREATE_NEW` refuses an existing name).
#[cfg(windows)]
pub(crate) fn create_new_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit_handle: i32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const c_void,
            disposition: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
    }
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const READ_CONTROL: u32 = 0x0002_0000;
    const DELETE: u32 = 0x0001_0000;
    const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x7;
    const CREATE_NEW: u32 = 1;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

    let descriptor = SecurityDescriptor::owner_only(path, false).map_err(std::io::Error::other)?;
    let attributes = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor: descriptor.0,
        inherit_handle: 0,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if wide[..wide.len() - 1].contains(&0) {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    // SAFETY: `wide` is NUL-terminated and `attributes` (with the descriptor it points to) outlives
    // the call; the handle is owned by the returned `File` on success.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE | READ_CONTROL | DELETE,
            FILE_SHARE_READ_WRITE_DELETE,
            (&attributes as *const SecurityAttributes).cast(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a fresh handle that nothing else owns.
    let file = unsafe { std::fs::File::from_raw_handle(handle) };
    if let Err(error) = verify_owner_only_handle(&file, path) {
        let _ = discard_unverified_owner_only(&file, path);
        return Err(error.into());
    }
    Ok(file)
}

/// Mark an empty just-created object for deletion through its current handle. No path lookup runs
/// between failed privacy verification and cleanup.
#[cfg(windows)]
pub(crate) fn discard_unverified_owner_only(
    file: &std::fs::File,
    shown: &Path,
) -> crate::error::Result<()> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetFileInformationByHandle(
            file: *mut std::ffi::c_void,
            class: i32,
            info: *const std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    const FILE_DISPOSITION_INFO: i32 = 4;
    let delete: u8 = 1;
    // SAFETY: live handle was opened with DELETE; BOOL buffer is correctly sized.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FILE_DISPOSITION_INFO,
            (&delete as *const u8).cast(),
            1,
        )
    } == 0
    {
        return Err(FsError::io(shown, std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Create a private directory through its retained native handle. Verification and cleanup use
/// that exact created object, so a namespace replacement cannot turn failed verification into
/// deletion of another directory.
#[cfg(windows)]
fn create_dir_owner_only(path: &Path) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }
    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: *mut c_void,
        object_name: *mut UnicodeString,
        attributes: u32,
        security_descriptor: *mut c_void,
        security_quality_of_service: *mut c_void,
    }
    #[repr(C)]
    struct IoStatusBlock {
        status: usize, // NTSTATUS / pointer union; matches the other NtCreateFile declarations.
        information: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const c_void,
            disposition: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
        fn GetFileInformationByHandleEx(
            file: *mut c_void,
            class: i32,
            info: *mut c_void,
            size: u32,
        ) -> i32;
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file: *mut *mut c_void,
            access: u32,
            attributes: *mut ObjectAttributes,
            status: *mut IoStatusBlock,
            allocation_size: *const i64,
            file_attributes: u32,
            share: u32,
            disposition: u32,
            options: u32,
            ea_buffer: *const c_void,
            ea_length: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }
    const FILE_ADD_SUBDIRECTORY: u32 = 0x4;
    const READ_ATTRIBUTES: u32 = 0x80;
    const READ_CONTROL: u32 = 0x0002_0000;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const DELETE: u32 = 0x0001_0000;
    const OPEN_EXISTING: u32 = 3;
    const FILE_SHARE_READ_WRITE: u32 = 0x3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_TAG_INFO: i32 = 9;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_CREATE: u32 = 2;
    const FILE_DIRECTORY_FILE: u32 = 0x1;
    const FILE_WRITE_THROUGH: u32 = 0x2;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
    const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    #[repr(C)]
    struct AttributeTagInfo {
        attributes: u32,
        tag: u32,
    }

    let parent = match path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => parent.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let mut child: Vec<u16> = name.encode_wide().collect();
    if child.is_empty()
        || child
            .iter()
            .any(|unit| *unit == 0 || *unit == b'\\' as u16 || *unit == b'/' as u16)
    {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let child_bytes = u16::try_from(child.len() * 2)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let mut child_name = UnicodeString {
        length: child_bytes,
        maximum_length: child_bytes,
        buffer: child.as_mut_ptr(),
    };
    let parent_wide: Vec<u16> = parent.as_os_str().encode_wide().chain(Some(0)).collect();
    if parent_wide[..parent_wide.len() - 1].contains(&0) {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    // Another creator can still hold DELETE on this freshly created parent while
    // verifying it (and retaining the ability to discard a failed creation).
    // Keep our no-delete-sharing pin; wait only for that transient sharing refusal.
    // Never turn access denial, a missing parent, or a persistent lock into success.
    let mut attempt = 0;
    let parent_raw = loop {
        // SAFETY: NUL-terminated parent path; a successful handle is adopted below.
        let raw = unsafe {
            CreateFileW(
                parent_wide.as_ptr(),
                FILE_ADD_SUBDIRECTORY | READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_SHARE_READ_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if !raw.is_null() && raw as isize != -1 {
            break raw;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(32) || attempt + 1 >= crate::replaced::MAX_OPEN_TRIES {
            return Err(error);
        }
        std::thread::sleep(crate::replaced::pause_after(attempt));
        attempt += 1;
    };
    // SAFETY: CreateFileW returned an owned parent-directory handle.
    let parent_file = unsafe { std::fs::File::from_raw_handle(parent_raw) };
    let mut tag = AttributeTagInfo {
        attributes: 0,
        tag: 0,
    };
    // SAFETY: parent handle is live and output buffer has exact AttributeTagInfo size.
    if unsafe {
        GetFileInformationByHandleEx(
            parent_file.as_raw_handle(),
            FILE_ATTRIBUTE_TAG_INFO,
            (&mut tag as *mut AttributeTagInfo).cast(),
            std::mem::size_of::<AttributeTagInfo>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if tag.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(FsError::Symlink(parent).into());
    }
    let descriptor = SecurityDescriptor::owner_only(path, true).map_err(std::io::Error::from)?;
    let mut attributes = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: parent_file.as_raw_handle(),
        object_name: &mut child_name,
        attributes: 0x40, // OBJ_CASE_INSENSITIVE
        security_descriptor: descriptor.0,
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut status = IoStatusBlock {
        status: 0,
        information: 0,
    };
    let mut raw = std::ptr::null_mut();
    // SAFETY: pinned parent, one checked component, and descriptor buffers all remain live.
    let result = unsafe {
        NtCreateFile(
            &mut raw,
            READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE | DELETE,
            &mut attributes,
            &mut status,
            std::ptr::null(),
            FILE_ATTRIBUTE_DIRECTORY,
            FILE_SHARE_READ_WRITE,
            FILE_CREATE,
            FILE_DIRECTORY_FILE
                | FILE_WRITE_THROUGH
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_REPARSE_POINT,
            std::ptr::null(),
            0,
        )
    };
    if result < 0 {
        // SAFETY: RtlNtStatusToDosError converts this API's NTSTATUS result.
        return Err(std::io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    if raw.is_null() || raw as isize == -1 {
        return Err(std::io::Error::from(std::io::ErrorKind::Other));
    }
    // SAFETY: NtCreateFile returned one owned handle to precisely the object it created.
    let file = unsafe { std::fs::File::from_raw_handle(raw) };
    if let Err(error) = verify_owner_only_handle(&file, path) {
        let _ = discard_unverified_owner_only(&file, path);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod owner_policy_tests {
    use super::*;

    const USER: &str = "S-1-5-21-1-2-3-1001";
    const ADMINS: &str = "S-1-5-32-544";
    const SYSTEM: &str = "S-1-5-18";
    const OTHER_USER: &str = "S-1-5-21-1-2-3-1002";

    const EVERYONE: &str = "S-1-1-0";
    const LOCAL: &str = "S-1-2-0";
    const INTERACTIVE: &str = "S-1-5-4";
    const AUTH_USERS: &str = "S-1-5-11";
    const USERS: &str = "S-1-5-32-545";
    const ENABLED_GROUP: u32 = SE_GROUP_ENABLED | 0x1; // ENABLED | MANDATORY

    fn broad(sid: &&str) -> bool {
        match *sid {
            "S-1-1-0" => sid_is_broad(1, &[0]),
            "S-1-2-0" => sid_is_broad(2, &[0]),
            "S-1-5-4" => sid_is_broad(5, &[4]),
            "S-1-5-11" => sid_is_broad(5, &[11]),
            "S-1-5-32-545" => sid_is_broad(5, &[32, 545]),
            _ => false,
        }
    }

    fn trusted(owner: &&str, groups: &[(&str, u32)]) -> bool {
        owner_is_trusted(owner, &USER, groups, &broad)
    }

    #[test]
    fn current_user_owner_is_trusted() {
        assert!(trusted(&USER, &[]));
    }

    #[test]
    fn administrators_owner_needs_owner_and_enabled_flags() {
        assert!(trusted(
            &ADMINS,
            &[(ADMINS, SE_GROUP_OWNER | SE_GROUP_ENABLED)]
        ));
        // Enabled but not an owner group: rejected.
        assert!(!trusted(&ADMINS, &[(ADMINS, SE_GROUP_ENABLED)]));
        // Owner but not enabled (filtered token, deny-only): rejected.
        assert!(!trusted(
            &ADMINS,
            &[(ADMINS, SE_GROUP_OWNER | SE_GROUP_USE_FOR_DENY_ONLY)]
        ));
        assert!(!trusted(
            &ADMINS,
            &[(
                ADMINS,
                SE_GROUP_OWNER | SE_GROUP_ENABLED | SE_GROUP_USE_FOR_DENY_ONLY
            )]
        ));
        assert!(!trusted(&ADMINS, &[]));
    }

    #[test]
    fn broad_groups_are_never_trusted_owners() {
        for sid in [EVERYONE, LOCAL, INTERACTIVE, AUTH_USERS, USERS] {
            // Enabled and mandatory, not an owner group.
            assert!(!trusted(&sid, &[(sid, ENABLED_GROUP)]), "{sid}");
            // Hard-deny holds even when the token flags the group as an owner.
            assert!(
                !trusted(&sid, &[(sid, ENABLED_GROUP | SE_GROUP_OWNER)]),
                "{sid}"
            );
        }
    }

    #[test]
    fn broad_sid_shapes() {
        assert!(sid_is_broad(1, &[0]) && sid_is_broad(5, &[32, 545]));
        assert!(!sid_is_broad(5, &[32, 544]) && !sid_is_broad(5, &[18]));
    }

    #[test]
    fn foreign_owner_is_rejected() {
        let groups = [(ADMINS, SE_GROUP_OWNER | SE_GROUP_ENABLED)];
        assert!(!trusted(&OTHER_USER, &groups));
        assert!(!trusted(&SYSTEM, &groups));
    }
}

/// The tolerant DACL policy (rightkit#517 / #676), pure so it runs on every platform: which ACEs may
/// appear on an object that still counts as owner-only.
#[cfg(test)]
mod dacl_policy_tests {
    use super::*;

    const SYSTEM: (u64, &[u32]) = (5, &[18]);
    const ADMINS: (u64, &[u32]) = (5, &[32, 544]);
    const CREATOR_OWNER: (u64, &[u32]) = (3, &[0]);
    const EVERYONE: (u64, &[u32]) = (1, &[0]);
    const USERS: (u64, &[u32]) = (5, &[32, 545]);
    const AUTH_USERS: (u64, &[u32]) = (5, &[11]);
    const INTERACTIVE: (u64, &[u32]) = (5, &[4]);
    const OTHER_USER: (u64, &[u32]) = (5, &[21, 1, 2, 3, 1002]);
    const ALLOW: u8 = 0;
    const DENY: u8 = 1;
    const INHERITED: u8 = 0x10;
    const OICI: u8 = 0x03;

    fn allow(trustee: (u64, &[u32]), is_user: bool, flags: u8) -> bool {
        ace_is_acceptable(ALLOW, flags, Some((is_user, trustee.0, trustee.1)))
    }

    #[test]
    fn the_current_user_system_and_administrators_are_accepted_inherited_or_explicit() {
        // %LOCALAPPDATA% inherits exactly these three.
        assert!(allow(OTHER_USER, true, INHERITED | OICI));
        assert!(allow(SYSTEM, false, INHERITED | OICI));
        assert!(allow(ADMINS, false, INHERITED | OICI));
        assert!(allow(SYSTEM, false, 0));
        assert!(allow(ADMINS, false, OICI));
    }

    #[test]
    fn another_user_or_a_broad_group_is_rejected_whatever_the_flags() {
        for (name, trustee) in [
            ("other user", OTHER_USER),
            ("Everyone", EVERYONE),
            ("Users", USERS),
            ("Authenticated Users", AUTH_USERS),
            ("INTERACTIVE", INTERACTIVE),
        ] {
            for flags in [0, INHERITED, OICI, INHERITED | OICI, INHERIT_ONLY_ACE] {
                assert!(!allow(trustee, false, flags), "{name} flags {flags:#x}");
            }
        }
    }

    #[test]
    fn creator_owner_counts_only_when_inherit_only() {
        assert!(allow(CREATOR_OWNER, false, INHERIT_ONLY_ACE | OICI));
        assert!(!allow(CREATOR_OWNER, false, OICI));
        assert!(!allow(CREATOR_OWNER, false, 0));
    }

    #[test]
    fn deny_aces_are_ignored_and_unknown_types_fail_closed() {
        for deny in [1u8, 6, 10, 12] {
            assert!(ace_is_acceptable(deny, 0, None), "deny type {deny}");
            assert!(ace_is_acceptable(
                deny,
                0,
                Some((false, EVERYONE.0, EVERYONE.1))
            ));
        }
        // Object allow (5), compound allow (4), unknown (0x7f), system audit (2), mandatory label (17).
        for other in [2u8, 3, 4, 5, 7, 8, 11, 13, 14, 15, 16, 17, 0x7f, 0xff] {
            assert!(
                !ace_is_acceptable(other, 0, Some((true, 5, &[18]))),
                "type {other}"
            );
        }
        assert_eq!(ace_kind(ALLOW), AceKind::Allow);
        assert_eq!(ace_kind(9), AceKind::Allow);
        assert_eq!(ace_kind(DENY), AceKind::Deny);
    }

    #[test]
    fn an_allow_ace_with_an_unreadable_trustee_is_rejected() {
        assert!(!ace_is_acceptable(ALLOW, 0, None));
        assert!(!ace_is_acceptable(9, OICI, None));
    }

    #[test]
    fn a_broad_trustee_cannot_pose_as_the_current_user_by_authority() {
        // Only the explicit `is_current_user` bit (an `EqualSid` against the token) counts.
        assert!(!allow(EVERYONE, false, 0));
        assert!(!allow((5, &[18, 1]), false, 0));
        assert!(!allow((5, &[32, 544, 1]), false, 0));
        assert!(!allow((5, &[]), false, 0));
        assert!(!allow((6, &[18]), false, 0));
    }
}

#[cfg(all(test, unix))]
mod verify_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_private_file_verifies_and_a_broad_one_is_too_permissive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        verify_owner_only(&path).unwrap();
        assert!(is_owner_only_strict(&path).unwrap());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            verify_owner_only(&path),
            Err(FsError::TooPermissive(_))
        ));
        assert!(!is_owner_only_strict(&path).unwrap());
    }

    #[test]
    fn a_symlink_is_never_followed_by_the_verify() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("t");
        std::fs::write(&target, b"x").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(verify_owner_only(&link), Err(FsError::Symlink(_))));
    }

    #[test]
    fn a_missing_path_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(verify_owner_only(&dir.path().join("absent"))
            .unwrap_err()
            .is_not_found());
    }
}
