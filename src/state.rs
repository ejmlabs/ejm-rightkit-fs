//! A descriptor-pinned state directory.
//!
//! On unix every operation goes through one verified directory descriptor: the directory is
//! opened component by component with `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`, `fstat`-checked
//! (a directory, owned by the effective user, not writable by group/others) and compared with an
//! `lstat` taken before the walk. Reads, writes, renames, unlinks and listings are `*at`
//! operations relative to that descriptor, so renaming or swapping the path afterwards (or
//! planting a symlink at a child name) cannot redirect anything. Donor: Cockpit
//! `core/src/store.rs` (pinned descriptor, tested against swaps).
//!
//! Windows walks components without following reparse points, then retains a directory handle
//! without delete sharing. Child opens use `NtCreateFile` relative to that handle; publication
//! and removal use handle-based rename/disposition. New entries have an owner-only DACL at
//! creation. Reparse-point children are refused, including removal. Other targets retain a
//! path-based fallback.
//!
//! # Durability
//!
//! [`StateDir::write`], [`write_new`](StateDir::write_new), [`remove`](StateDir::remove) and
//! [`child`](StateDir::child) (when it creates) are durable operations, and a durability failure
//! is an error, never swallowed:
//!
//! - Unix: the file is flushed before it is published, and the directory is flushed after the
//!   publish, removal or creation. A failed directory flush is
//!   [`FsError::ReplacedButDirSyncFailed`] (writes) or [`FsError::DirSyncFailed`] (removal,
//!   creation); the change is in place but may not survive power loss. On Apple targets the flush
//!   is `F_FULLFSYNC`, falling back to `F_BARRIERFSYNC` then `fsync` unless
//!   [`StateOptions::sync`] is [`SyncPolicy::Strict`]; the `*_reporting` writes return the
//!   [`SyncLevel`] reached. [`StateDir::create`] and [`StateDir::child`] publish the link of every
//!   directory **they create**: before descending into it they sync its *parent* (see the table
//!   below), so a later durable write into the leaf cannot lose a link this call made. A
//!   directory that already exists is not synced, and neither is one a racing creator made first.
//!   A failed flush stops the walk with [`FsError::DirSyncFailed`] (the directory exists; see the
//!   residual in the `atomic` module docs, section 3).
//! - Windows: there is no directory flush. `FlushFileBuffers` needs `GENERIC_WRITE`, and on a
//!   directory handle opened for listing (the pinned handle) it fails with
//!   `ERROR_ACCESS_DENIED` (Pulse measured this on NTFS), so the crate does not call it. The
//!   file is flushed before publication, and directory creation (including every missing
//!   ancestor [`StateDir::create`] makes), the publishing rename and the removing delete run on
//!   handles opened `FILE_WRITE_THROUGH`, which asks the filesystem to commit the operation
//!   before the call returns. That is the strongest equivalent available without a directory
//!   flush; Microsoft does not document a guarantee for directory-entry metadata, and a
//!   power-loss test of the claim has not been run.
//!
//! # Directory creation: every path, what it syncs, what a crash leaves
//!
//! The rule, the same as `atomic::create_dir_all_durable` (which the durable `create_parents`
//! atomic write and the settings crate use): a directory's link is synced only when **this call
//! created it** (the `mkdirat` returned success, not `EEXIST`). There is no in-process record of
//! earlier syncs and no walk over directories that already exist, so a call whose directories all
//! exist makes no ancestor sync. The residual (a directory created by an earlier call that died
//! before its parent sync, or whose sync failed, is not synced by later calls) is stated once, in
//! the `atomic` module docs, section 3.
//!
//! | Path | Platform | Component state | Creation call and mode | Sync | Crash between steps leaves | Error | Test |
//! |---|---|---|---|---|---|---|---|
//! | `StateDir::create` / `create_with` (`pin`, create) | Unix | missing, this call's `mkdirat` succeeds | `mkdirat` `0700` below the verified parent descriptor | parent descriptor synced before descending | before the sync: the directory may be absent after reboot, nothing was written below it; after: present | [`FsError::DirSyncFailed`] (the directory stays) | `tests/state_ancestors.rs::create_syncs_the_parent_of_each_new_directory_before_descending`, `a_failed_ancestor_sync_stops_the_walk_with_a_typed_error` |
//! | same | Unix | exists (made by an earlier call, another process, a crashed run, or moved into place) | none | none: a second identical call syncs nothing | n/a (see the residual) | none | `state_ancestors.rs::a_second_identical_create_syncs_nothing`, `a_directory_made_by_another_process_is_found_not_synced`, `a_directory_moved_into_place_is_found_not_synced` |
//! | same | Unix | missing, but a racing creator's `mkdirat` wins (`EEXIST`) | none | none: theirs to publish | n/a | none | none: needs a racing creator |
//! | same | Unix | exists, a symlink | none | refused (`O_NOFOLLOW`) | n/a | [`FsError::Symlink`] | `tests/e2e.rs::symlink_attacks_on_the_state_dir_are_refused` |
//! | same | Unix, macOS | missing, parent ACL has an inheritable `allow` entry | refused before `mkdirat`; a directory that got an `allow` entry anyway is removed (empty) and refused; deny-only ACLs pass | none | nothing created | [`FsError::UnsafeAcl`] | `tests/acl_macos.rs::state_dir_refuses_an_inheriting_parent` |
//! | `StateDir::child` | Unix | missing / exists | as `pin` above, relative to this directory: a created child is synced through its parent, an existing one is returned as it is | as `pin` above | as `pin` above | `DirSyncFailed` | `state_ancestors.rs::a_child_created_through_the_handle_is_published_through_its_parent`, `a_child_made_by_another_process_is_found_not_synced`, `state_durability.rs::dir_sync::creating_a_child_reports_a_failed_directory_sync_and_a_retry_does_not_sync_it_again` |
//! | `StateDir::create` / `child` | Windows | missing | `NtCreateFile` `FILE_OPEN_IF` with `FILE_WRITE_THROUGH`, owner-only DACL at creation | none exists; write-through on the creating handle | creation committed before the call returns, by the filesystem's write-through contract (no power-loss test has been run) | none | cannot be tested without power-loss injection; the flag is `src/state/windows_tests.rs::directory_removal_and_creation_need_no_directory_flush` shows creation succeeds without a flush call |
//! | same | Windows | exists | none | none | n/a | none | `windows_tests.rs::native_temp_creation_and_relative_publication` |

use crate::atomic::{durable_sync_with, Privacy, SyncLevel, SyncPolicy, TEMP_MARKER};
use crate::error::{FsError, Result};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The longest single path component the platform accepts: 255 bytes on unix, 255 UTF-16 code
/// units on Windows. Reads, listings, stats and removals accept any name up to this length.
pub const PLATFORM_NAME_LIMIT: usize = 255;

/// The default cap on names a `StateDir` *creates*: 200 bytes on unix, 255 UTF-16 units on
/// Windows. On unix 200 leaves room for the `.<name>.rktmp-<pid>-<n>-<attempt>` temp name (at
/// most 42 bytes beyond the name) inside 255 for names written by older versions; temp names now
/// carry at most 64 bytes of the name, so [`StateOptions::max_name_len`] can raise the cap to
/// [`PLATFORM_NAME_LIMIT`].
#[cfg(not(windows))]
pub const DEFAULT_WRITE_NAME_LEN: usize = 200;
/// See the unix constant: Windows has always accepted names up to the platform limit.
#[cfg(windows)]
pub const DEFAULT_WRITE_NAME_LEN: usize = PLATFORM_NAME_LIMIT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub struct EntryStat {
    pub kind: EntryKind,
    pub size: u64,
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub modified_unix_ms: u64,
}

/// One directory entry from [`StateDir::list_entries`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// The entry name. A name that is not valid Unicode is returned lossily (U+FFFD) and cannot
    /// be addressed by the other methods.
    pub name: String,
    /// What the entry is, without following it. On Windows any reparse point (symlink, junction,
    /// mount point, placeholder) is [`EntryKind::Symlink`].
    pub kind: EntryKind,
}

/// How a [`StateDir`] is opened or created. `StateOptions::default()` is what
/// [`StateDir::open`] and [`StateDir::create`] use. Children made by [`StateDir::child`] inherit
/// the options.
#[derive(Clone, Debug)]
pub struct StateOptions {
    /// Windows access control on directories and files this `StateDir` creates (including
    /// missing ancestors):
    ///
    /// - [`Privacy::OwnerOnly`] (default): a protected owner-only ACL, set at creation. Beware
    ///   that if the `StateDir` is the first creator of an application data root, that root gets
    ///   the protected ACL too and every later child created under it inherits it (other
    ///   accounts, services and backup agents lose access). Create the data root first, or use
    ///   the option below.
    /// - [`Privacy::OwnerOnlyUnixInheritWindows`] or [`Privacy::Inherit`]: no ACL step on
    ///   Windows, so new objects carry the parent's inherited ACL, the same meaning these have for
    ///   atomic writes.
    ///
    /// Unix ignores this: directories are always created `0700`, files `0600`, and a pinned
    /// directory must be owned by the user and not group/other writable.
    /// [`StateDir::create_new_empty`] requires `OwnerOnly` on Windows and refuses inheritance.
    pub privacy: Privacy,
    /// Windows only: open the pinned directory with `FILE_SHARE_DELETE`, so other handles may
    /// rename or delete the directory (or an ancestor) while this `StateDir` is open. Operations
    /// stay handle-relative and keep acting on the original directory, but
    /// [`StateDir::path`] then names whatever is at that path now, so use no path-based call on
    /// it. Default `false`: the directory cannot be renamed or removed while open, so the path
    /// stays true. Unix descriptors always behave as if this were `true`.
    pub allow_rename_while_open: bool,
    /// Longest name this `StateDir` creates, clamped to `1..=`[`PLATFORM_NAME_LIMIT`]. `None` is
    /// [`DEFAULT_WRITE_NAME_LEN`]. Reads and listings are never limited below the platform limit.
    pub max_name_len: Option<usize>,
    /// Behaviour when `F_FULLFSYNC` is unavailable (Apple only). Default
    /// [`SyncPolicy::Fallback`]; [`SyncPolicy::Strict`] makes writes fail instead.
    pub sync: SyncPolicy,
}

impl Default for StateOptions {
    fn default() -> Self {
        Self {
            privacy: Privacy::OwnerOnly,
            allow_rename_while_open: false,
            max_name_len: None,
            sync: SyncPolicy::Fallback,
        }
    }
}

impl StateOptions {
    fn write_cap(&self) -> usize {
        self.max_name_len
            .unwrap_or(DEFAULT_WRITE_NAME_LEN)
            .clamp(1, PLATFORM_NAME_LIMIT)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn inherit_acl(&self) -> bool {
        self.privacy != Privacy::OwnerOnly
    }
}

fn check_name(name: &str, limit: usize) -> Result<()> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.bytes().any(|b| b == b'/' || b == b'\\' || b == 0);
    #[cfg(not(windows))]
    let bad = bad || name.len() > limit;
    #[cfg(windows)]
    let bad = bad || !sys::valid_component(name, limit);
    if bad {
        return Err(FsError::BadName(name.to_string()));
    }
    Ok(())
}

/// Entry names are single path components: no separators, no NUL, not `.`/`..`, within the
/// default write cap ([`DEFAULT_WRITE_NAME_LEN`]), and not shaped like a temp name this crate
/// generates (`.<name>.rktmp-<pid>-<n>-<attempt>`), which a sweep would delete.
pub fn validate_name(name: &str) -> Result<()> {
    check_name(name, DEFAULT_WRITE_NAME_LEN)?;
    crate::atomic::reject_reserved_name(name)
}

/// The prefix of `name` used in a temp name: at most 64 bytes, cut at a character boundary, so a
/// temp name fits the platform limit however long the real name is.
fn temp_prefix(name: &str) -> &str {
    let mut end = name.len().min(64);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// `EntryKind` from Windows file attributes: reparse point, then directory, else file.
#[cfg_attr(not(windows), allow(dead_code))]
fn kind_from_attributes(attributes: u32) -> EntryKind {
    const DIRECTORY: u32 = 0x10;
    const REPARSE_POINT: u32 = 0x400;
    if attributes & REPARSE_POINT != 0 {
        EntryKind::Symlink
    } else if attributes & DIRECTORY != 0 {
        EntryKind::Dir
    } else {
        EntryKind::File
    }
}

/// Flush `file`, honouring the policy. Under the `testing` feature a thread can pretend the
/// strongest flush is unavailable ([`testing::without_full_sync`]).
fn flush(file: &File, policy: SyncPolicy) -> std::io::Result<SyncLevel> {
    #[cfg(feature = "testing")]
    if testing::full_sync_unavailable() {
        return crate::atomic::sync_ladder(
            policy,
            || Err(std::io::Error::from(std::io::ErrorKind::Unsupported)),
            || file.sync_all(),
            || file.sync_all(),
        );
    }
    durable_sync_with(file, policy)
}

#[derive(Debug)]
pub struct StateDir {
    dir: sys::Dir,
    path: PathBuf,
    options: StateOptions,
    /// Opened by [`StateDir::open_shared`]: the pinned directory (and children found below it) need
    /// not be private. Inherited by [`StateDir::child`].
    shared: bool,
}

impl StateDir {
    /// Pin an existing directory.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, &StateOptions::default())
    }

    /// [`open`](Self::open) with explicit [`StateOptions`].
    pub fn open_with(path: &Path, options: &StateOptions) -> Result<Self> {
        Ok(Self {
            dir: sys::Dir::pin(path, false, options, false)?,
            path: path.to_path_buf(),
            options: options.clone(),
            shared: false,
        })
    }

    /// Pin an existing directory **that need not be private**: [`open`](Self::open) refuses a
    /// directory writable by group or others ([`FsError::TooPermissive`]); this accepts it. For a
    /// directory the user chose and this program does not own the policy of (an attachment folder, a
    /// downloads folder, a shared workspace), where the private-directory rule would make the pin
    /// impossible.
    ///
    /// Everything else is as strict as [`open`](Self::open): every component is opened without
    /// following a symlink, the leaf must be a real directory owned by the effective user (Unix), and
    /// every operation stays descriptor- or handle-relative to the pinned directory, so a swapped or
    /// symlinked path cannot redirect it. What is given up is only the assumption that nobody else
    /// can change the directory's *entries*: another user able to write there can create, rename or
    /// remove names (a world-writable directory without the sticky bit even lets them replace yours).
    /// Reads still refuse symlinks and non-regular files; write only what you can lose. Children made
    /// by [`child`](Self::child) inherit this policy. Windows checks no ACL or mode when pinning, so
    /// there this is the same as [`open`](Self::open).
    pub fn open_shared(path: &Path) -> Result<Self> {
        Self::open_shared_with(path, &StateOptions::default())
    }

    /// [`open_shared`](Self::open_shared) with explicit [`StateOptions`].
    pub fn open_shared_with(path: &Path, options: &StateOptions) -> Result<Self> {
        Ok(Self {
            dir: sys::Dir::pin(path, false, options, true)?,
            path: path.to_path_buf(),
            options: options.clone(),
            shared: true,
        })
    }

    /// Create the directory (and missing ancestors, owner-only, through descriptor-relative
    /// `mkdirat`) if needed, then pin it.
    pub fn create(path: &Path) -> Result<Self> {
        Self::create_with(path, &StateOptions::default())
    }

    /// [`create`](Self::create) with explicit [`StateOptions`].
    pub fn create_with(path: &Path, options: &StateOptions) -> Result<Self> {
        Ok(Self {
            dir: sys::Dir::pin(path, true, options, false)?,
            path: path.to_path_buf(),
            options: options.clone(),
            shared: false,
        })
    }

    /// Like [`create`](Self::create) but resolves symlinks in the *parent* first (for example
    /// macOS `/var` -> `/private/var`). The final component must still be a real directory.
    pub fn create_in_resolved_parent(path: &Path) -> Result<Self> {
        Self::create_in_resolved_parent_with(path, &StateOptions::default())
    }

    /// [`create_in_resolved_parent`](Self::create_in_resolved_parent) with explicit
    /// [`StateOptions`].
    pub fn create_in_resolved_parent_with(path: &Path, options: &StateOptions) -> Result<Self> {
        let name = path
            .file_name()
            .ok_or_else(|| FsError::BadName(path.display().to_string()))?;
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => {
                p.canonicalize().map_err(|e| FsError::io(p, e))?
            }
            _ => std::env::current_dir().map_err(|e| FsError::io(path, e))?,
        };
        Self::create_with(&parent.join(name), options)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The options this directory was opened with.
    pub fn options(&self) -> &StateOptions {
        &self.options
    }

    /// Create-or-open a direct child directory (owner-only), pinned relative to this one. The
    /// name is held to the write cap, since the call may create it. A directory this call
    /// creates is made durable like a write: on unix this directory is synced, and a failure is
    /// [`FsError::DirSyncFailed`] with the child already created. A child that already exists is
    /// returned as it is, with no sync (see the module table), and so is one a racing creator
    /// made first.
    pub fn child(&self, name: &str) -> Result<StateDir> {
        check_name(name, self.options.write_cap())?;
        Ok(Self {
            dir: self.dir.child(name, self.options.sync, self.shared)?,
            path: self.path.join(name),
            options: self.options.clone(),
            shared: self.shared,
        })
    }

    pub fn stat(&self, name: &str) -> Result<Option<EntryStat>> {
        check_name(name, PLATFORM_NAME_LIMIT)?;
        self.dir.stat(name)
    }

    pub fn exists(&self, name: &str) -> Result<bool> {
        Ok(self.stat(name)?.is_some())
    }

    /// Read a regular file (never through a symlink), refusing anything over `max_bytes`. Any
    /// existing name up to [`PLATFORM_NAME_LIMIT`] can be read, including one longer than this
    /// directory's write cap.
    pub fn read(&self, name: &str, max_bytes: u64) -> Result<Vec<u8>> {
        check_name(name, PLATFORM_NAME_LIMIT)?;
        let path = self.path.join(name);
        let mut file = self.open_read(name, &crate::handle::ReadOptions::new())?;
        let meta = file.metadata().map_err(|e| FsError::io(&path, e))?;
        if meta.len() > max_bytes {
            return Err(FsError::TooLarge {
                path,
                size: meta.len(),
                limit: max_bytes,
            });
        }
        let mut out = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
        // One byte past the limit proves the file outgrew it. `u64::MAX` means "no limit", so
        // the sum saturates instead of wrapping to a zero-byte read.
        (&mut file)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut out)
            .map_err(|e| FsError::io(&path, e))?;
        if out.len() as u64 > max_bytes {
            return Err(FsError::TooLarge {
                path,
                size: out.len() as u64,
                limit: max_bytes,
            });
        }
        Ok(out)
    }

    /// Open a regular file in this directory for reading and return the handle.
    ///
    /// The entry is opened relative to the pinned directory without following a symlink or reparse
    /// point ([`FsError::Symlink`]); anything that is not a regular file is [`FsError::NotAFile`] (a
    /// FIFO or device is never waited on); [`ReadOptions`](crate::handle::ReadOptions) adds the
    /// checks for more than one hard link ([`FsError::HardLinked`]), owner-only access on the open
    /// handle, and, on Windows, read-only sharing
    /// (no other handle may write, delete or rename the file while it is open). The handle is the
    /// caller's to read, query ([`crate::handle::final_path`], [`crate::FileKey::of_file`]) and drop.
    /// Any existing name up to [`PLATFORM_NAME_LIMIT`] can be opened.
    pub fn open_read(&self, name: &str, options: &crate::handle::ReadOptions) -> Result<File> {
        check_name(name, PLATFORM_NAME_LIMIT)?;
        let shown = self.path.join(name);
        let file = self.dir.open_read_with(name, options)?;
        crate::handle::finish_read_open(file, &shown, options)
    }

    /// Remove an **empty directory** child. A symlink, junction or file at `name` is refused (a
    /// symlink is [`FsError::Symlink`] on Windows and `NotADirectory` on unix: it is never followed
    /// and never removed here, use [`remove_link`](Self::remove_link)); a directory that still holds
    /// entries fails with the OS error (`ENOTEMPTY`, `ERROR_DIR_NOT_EMPTY`). Missing is `Ok(false)`.
    /// Durable like [`remove`](Self::remove): a failed directory sync is [`FsError::DirSyncFailed`]
    /// with the directory already gone.
    pub fn remove_dir(&self, name: &str) -> Result<bool> {
        check_name(name, PLATFORM_NAME_LIMIT)?;
        match self.dir.remove_dir(name) {
            Ok(()) => {}
            Err(e) if e.is_not_found() => return Ok(false),
            Err(e) => return Err(e),
        }
        self.dir
            .sync(self.options.sync)
            .map_err(|source| FsError::DirSyncFailed {
                path: self.path.join(name),
                source,
            })?;
        Ok(true)
    }

    /// Remove a **symlink or junction** child, the link itself and never its target. Unix unlinks the
    /// symlink; Windows opens the reparse point (not its target) and deletes it through that handle,
    /// which removes a junction or directory symlink without touching what it points at. An entry
    /// that is not a link (a file, a directory) is refused as `InvalidInput`, so this cannot remove
    /// real content by mistake. Missing is `Ok(false)`. Durable like [`remove`](Self::remove).
    pub fn remove_link(&self, name: &str) -> Result<bool> {
        check_name(name, PLATFORM_NAME_LIMIT)?;
        match self.dir.remove_link(name) {
            Ok(()) => {}
            Err(e) if e.is_not_found() => return Ok(false),
            Err(e) => return Err(e),
        }
        self.dir
            .sync(self.options.sync)
            .map_err(|source| FsError::DirSyncFailed {
                path: self.path.join(name),
                source,
            })?;
        Ok(true)
    }

    /// Like [`read`](Self::read) but `Ok(None)` when the entry does not exist.
    pub fn read_opt(&self, name: &str, max_bytes: u64) -> Result<Option<Vec<u8>>> {
        match self.read(name, max_bytes) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Exclusively create an empty regular file at its final name and return its write handle.
    ///
    /// This is for permanent lock files: creation stays relative to this pinned directory,
    /// never creates a second hard link, and never replaces an existing name. An existing name
    /// returns [`FsError::AlreadyExists`]. Names obey the write cap and reserved-temp rules.
    /// The returned handle passes the regular-file, single-link, and owner-only handle checks of
    /// [`open_read`](Self::open_read) with `require_single_link(true).require_owner_only(true)`.
    /// On Windows, [`StateOptions::privacy`] must be [`Privacy::OwnerOnly`]; an inheritance option
    /// is refused before creation. Platforms other than Unix and Windows are likewise refused
    /// before creation ([`FsError::Unavailable`]).
    ///
    /// No file or directory flush is performed. Use [`write_new`](Self::write_new) for durable
    /// content publication. This method preserves process-lifetime lock identity; it does not
    /// promise that the directory entry survives power loss.
    ///
    /// A check can fail after the name becomes visible. Such a failure leaves the empty entry
    /// in place, closes its handle, and returns the error: removing a visible lock could let
    /// another caller lock a different inode. An error is not permission to remove or replace
    /// the name. Existing locks must be opened through this same pinned directory with
    /// `require_single_link(true).require_owner_only(true)`: an earlier error may have left an
    /// unverified file. Neither an existing name nor a single link proves private access.
    /// All handle checks describe the state now; later changes are not prevented.
    pub fn create_new_empty(&self, name: &str) -> Result<File> {
        #[cfg(not(any(unix, windows)))]
        {
            let _ = name;
            Err(FsError::Unavailable(
                "direct private empty-file creation requires Unix or Windows",
            ))
        }
        #[cfg(any(unix, windows))]
        {
            #[cfg(windows)]
            if self.options.inherit_acl() {
                return Err(FsError::Unavailable(
                    "direct private empty-file creation requires Privacy::OwnerOnly",
                ));
            }
            check_name(name, self.options.write_cap())?;
            crate::atomic::reject_reserved_name(name)?;
            let shown = self.path.join(name);
            let file = self
                .dir
                .create_new(name, true)
                .map_err(|error| match error {
                    FsError::AlreadyExists(_) => FsError::AlreadyExists(shown.clone()),
                    other => other,
                })?;
            crate::handle::finish_read_open(
                file,
                &shown,
                &crate::handle::ReadOptions::new()
                    .require_single_link(true)
                    .require_owner_only(true),
            )
        }
    }

    /// Atomically create or replace `name` (owner-only, file and directory synced). A failed
    /// directory sync is [`FsError::ReplacedButDirSyncFailed`]: the content is in place but the
    /// rename may not survive power loss. Windows has no directory flush; see the module docs.
    pub fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        self.publish(name, bytes, true).map(|_| ())
    }

    /// [`write`](Self::write) that also reports the weakest [`SyncLevel`] reached by the file
    /// and directory syncs. Windows reports only the file flush.
    pub fn write_reporting(&self, name: &str, bytes: &[u8]) -> Result<SyncLevel> {
        self.publish(name, bytes, true)
    }

    /// Atomically create `name`, failing with [`FsError::AlreadyExists`] rather than replacing.
    /// A failed directory sync is [`FsError::ReplacedButDirSyncFailed`] (the entry exists).
    pub fn write_new(&self, name: &str, bytes: &[u8]) -> Result<()> {
        self.publish(name, bytes, false).map(|_| ())
    }

    /// [`write_new`](Self::write_new) that reports the weakest [`SyncLevel`] reached.
    pub fn write_new_reporting(&self, name: &str, bytes: &[u8]) -> Result<SyncLevel> {
        self.publish(name, bytes, false)
    }

    fn publish(&self, name: &str, bytes: &[u8], replace: bool) -> Result<SyncLevel> {
        check_name(name, self.options.write_cap())?;
        // A name shaped like one of our temp names would be deleted by `sweep_temps`.
        crate::atomic::reject_reserved_name(name)?;
        let final_path = self.path.join(name);
        let (temp, mut file) = self.create_temp(name)?;
        let result = (|| -> Result<SyncLevel> {
            file.write_all(bytes)
                .map_err(|e| FsError::io(&final_path, e))?;
            let file_level =
                flush(&file, self.options.sync).map_err(|e| FsError::io(&final_path, e))?;
            drop(file);
            if replace {
                // Never replace a symlinked or non-file destination through the rename.
                if let Some(existing) = self.dir.stat(name)? {
                    if existing.kind != EntryKind::File {
                        return Err(FsError::NotAFile(final_path.clone()));
                    }
                }
                self.dir.rename_replace(&temp, name)?;
            } else {
                self.dir.link_new(&temp, name).map_err(|e| match e {
                    FsError::AlreadyExists(_) => FsError::AlreadyExists(final_path.clone()),
                    other => other,
                })?;
                let _ = self.dir.unlink(&temp);
            }
            // Past this point the entry is published; a failure no longer removes it.
            match self.dir.sync(self.options.sync) {
                Ok(Some(dir_level)) => Ok(file_level.min(dir_level)),
                Ok(None) => Ok(file_level),
                Err(source) => Err(FsError::ReplacedButDirSyncFailed {
                    path: final_path.clone(),
                    source,
                }),
            }
        })();
        if result.is_err() {
            let _ = self.dir.unlink(&temp);
        }
        result
    }

    fn create_temp(&self, name: &str) -> Result<(String, File)> {
        let prefix = temp_prefix(name);
        for attempt in 0..crate::atomic::TEMP_ATTEMPTS {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let temp = format!(".{prefix}{TEMP_MARKER}{}-{n}-{attempt}", std::process::id());
            match self.dir.create_new(&temp, false) {
                Ok(file) => return Ok((temp, file)),
                Err(FsError::AlreadyExists(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(FsError::AlreadyExists(self.path.join(name)))
    }

    /// Remove a file. Unix removes a symlink itself; Windows refuses reparse points.
    /// Missing is `Ok(false)`. The removal is made durable like a write: a failed directory sync
    /// is [`FsError::DirSyncFailed`] with the entry already gone.
    pub fn remove(&self, name: &str) -> Result<bool> {
        check_name(name, PLATFORM_NAME_LIMIT)?;
        match self.dir.unlink(name) {
            Ok(()) => {}
            Err(e) if e.is_not_found() => return Ok(false),
            Err(e) => return Err(e),
        }
        self.dir
            .sync(self.options.sync)
            .map_err(|source| FsError::DirSyncFailed {
                path: self.path.join(name),
                source,
            })?;
        Ok(true)
    }

    /// Names of direct children, capped at `limit` entries (error past the cap). Every existing
    /// name is returned, including one longer than the write cap.
    pub fn list(&self, limit: usize) -> Result<Vec<String>> {
        Ok(self
            .dir
            .entries(limit)?
            .into_iter()
            .map(|entry| entry.name)
            .collect())
    }

    /// Direct children with their [`EntryKind`], capped at `limit` entries (error past the cap).
    /// The kind comes from the directory entry itself (`d_type` on unix, the attributes in
    /// `FILE_ID_BOTH_DIR_INFO` on Windows), so no per-entry stat is made; on unix an entry whose
    /// filesystem reports no type (`DT_UNKNOWN`) gets one descriptor-relative `fstatat`.
    pub fn list_entries(&self, limit: usize) -> Result<Vec<DirEntry>> {
        self.dir.entries(limit)
    }

    /// Remove generated regular temp files left by writers confirmed dead. `max_age` remains for
    /// source compatibility but cannot override liveness: current, live, and unqueryable PIDs
    /// survive regardless of age. Only complete generated temp names
    /// (`.<name>.rktmp-<pid>-<n>-<attempt>`, or the longer `atomic_write` form) are considered,
    /// and `write`/`write_new` refuse those names as targets, so a committed record is never
    /// swept. The pinned directory rechecks the observed regular-file identity before removal;
    /// Unix cannot make that comparison and `unlinkat` one operation against arbitrary concurrent
    /// path replacement. Returns how many were removed.
    pub fn sweep_temps(&self, max_age: std::time::Duration) -> Result<usize> {
        let _ = max_age;
        #[cfg(windows)]
        {
            self.dir.sweep_temps()
        }
        #[cfg(not(windows))]
        {
            self.sweep_temps_path()
        }
    }

    #[cfg(not(windows))]
    fn sweep_temps_path(&self) -> Result<usize> {
        let mut removed = 0;
        for DirEntry { name, .. } in self.dir.entries(usize::MAX)? {
            // Only a complete generated temp name is ever reclaimed; a committed record that
            // merely contains the marker (`invoice.rktmp-1-final`) is left alone.
            let Some(pid) = crate::atomic::generated_temp_pid(&name) else {
                continue;
            };
            if !crate::atomic::confirmed_dead_writer(pid) {
                continue;
            }
            // Check again directly before descriptor-relative deletion. A failed query is not
            // proof of death, even if the generated name is old.
            if crate::atomic::confirmed_dead_writer(pid)
                && self.dir.unlink_regular(&name).unwrap_or(false)
            {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// Fault injection for consumer tests (feature `testing`, off by default; debug builds only).
/// Hooks are per thread and removed when their guard drops.
#[cfg(feature = "testing")]
pub mod testing {
    use std::cell::{Cell, RefCell};
    use std::fs::File;
    use std::io;

    type EmptyCreateHook = Box<dyn Fn(&File) -> crate::Result<()>>;

    /// One directory-level step of a `StateDir` operation, in the order it happened. Recorded on
    /// Unix only (Windows makes no directory sync call).
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum DirEvent {
        /// `mkdirat` created a directory with this single-component name.
        Created(String),
        /// A directory sync was attempted on the directory whose inode number is this (whether or
        /// not [`fail_dir_sync`] made it fail).
        Synced(u64),
    }

    thread_local! {
        static EMPTY_CREATE_HOOK: RefCell<Option<EmptyCreateHook>> = const { RefCell::new(None) };
        static DIR_SYNC_FAILURE: RefCell<Option<io::ErrorKind>> = const { RefCell::new(None) };
        static DIR_EVENTS: RefCell<Vec<DirEvent>> = const { RefCell::new(Vec::new()) };
        static DIR_SYNCS: Cell<usize> = const { Cell::new(0) };
        static FULL_SYNC_UNAVAILABLE: Cell<bool> = const { Cell::new(false) };
        static FALLBACK_STATS: Cell<usize> = const { Cell::new(0) };
    }

    /// Removes the hook when dropped.
    pub struct HookGuard(fn());

    impl Drop for HookGuard {
        fn drop(&mut self) {
            (self.0)();
        }
    }

    /// Observe or fail direct empty-file creation immediately after the final name is created,
    /// before post-creation checks. The hook runs only for [`super::StateDir::create_new_empty`].
    /// Returning an error leaves the visible entry in place. This tests publication boundaries
    /// without a timing race or a different filesystem implementation.
    pub fn on_empty_created(hook: impl Fn(&File) -> crate::Result<()> + 'static) -> HookGuard {
        EMPTY_CREATE_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        HookGuard(|| EMPTY_CREATE_HOOK.with(|slot| *slot.borrow_mut() = None))
    }

    pub(super) fn after_empty_create(file: &File) -> crate::Result<()> {
        EMPTY_CREATE_HOOK.with(|slot| match slot.borrow().as_ref() {
            Some(hook) => hook(file),
            None => Ok(()),
        })
    }

    /// Make every directory sync this thread's `StateDir` operations perform fail with `kind`
    /// (standing in for the OS call) until the guard drops. Unix only: Windows makes no
    /// directory sync call.
    pub fn fail_dir_sync(kind: io::ErrorKind) -> HookGuard {
        DIR_SYNC_FAILURE.with(|f| *f.borrow_mut() = Some(kind));
        HookGuard(|| DIR_SYNC_FAILURE.with(|f| *f.borrow_mut() = None))
    }

    /// Pretend the strongest flush (`F_FULLFSYNC`) is unavailable for this thread's `StateDir`
    /// syncs, on any platform, so [`SyncPolicy`](crate::SyncPolicy) can be exercised.
    pub fn without_full_sync() -> HookGuard {
        FULL_SYNC_UNAVAILABLE.with(|f| f.set(true));
        HookGuard(|| FULL_SYNC_UNAVAILABLE.with(|f| f.set(false)))
    }

    /// Directory syncs attempted on this thread since it started.
    pub fn dir_sync_count() -> usize {
        DIR_SYNCS.with(Cell::get)
    }

    /// The directory creations and directory syncs this thread's `StateDir` operations made, in
    /// order, since the last call. Empties the log. Tests assert the order to prove a directory
    /// is published (its parent synced) before the walk descends into it.
    pub fn take_dir_events() -> Vec<DirEvent> {
        DIR_EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
    }

    /// Per-entry `fstatat` fallbacks made by listings on this thread (entries whose filesystem
    /// reported no type). Zero on filesystems that report `d_type`.
    pub fn fallback_stat_count() -> usize {
        FALLBACK_STATS.with(Cell::get)
    }

    #[cfg(unix)]
    pub(super) fn record_created(name: &str) {
        DIR_EVENTS.with(|events| {
            events
                .borrow_mut()
                .push(DirEvent::Created(name.to_string()))
        });
    }

    #[cfg(unix)]
    pub(super) fn before_dir_sync(dir: &std::fs::File) -> Option<io::Error> {
        use std::os::unix::fs::MetadataExt;
        DIR_SYNCS.with(|c| c.set(c.get() + 1));
        if let Ok(meta) = dir.metadata() {
            DIR_EVENTS.with(|events| events.borrow_mut().push(DirEvent::Synced(meta.ino())));
        }
        DIR_SYNC_FAILURE.with(|f| {
            f.borrow()
                .map(|kind| io::Error::new(kind, "injected directory sync failure"))
        })
    }

    #[cfg(unix)]
    pub(super) fn count_fallback_stat() {
        FALLBACK_STATS.with(|c| c.set(c.get() + 1));
    }

    pub(super) fn full_sync_unavailable() -> bool {
        FULL_SYNC_UNAVAILABLE.with(Cell::get)
    }
}

#[cfg(unix)]
mod sys {
    use super::{DirEntry, EntryKind, EntryStat, StateOptions};
    use crate::atomic::{SyncLevel, SyncPolicy};
    use crate::error::{FsError, Result};
    use std::ffi::{CStr, CString};
    use std::fs::File;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
    use std::path::Path;

    #[derive(Debug)]
    pub struct Dir(File);

    fn cname(name: &str) -> Result<CString> {
        CString::new(name).map_err(|_| FsError::BadName(name.to_string()))
    }

    fn errno_is(e: &io::Error, code: i32) -> bool {
        e.raw_os_error() == Some(code)
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    unsafe fn reset_errno() {
        unsafe { *libc::__error() = 0 };
    }
    // Bionic has no `__errno_location`; its accessor is `__errno`.
    #[cfg(target_os = "android")]
    unsafe fn reset_errno() {
        unsafe { *libc::__errno() = 0 };
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "android"
    )))]
    unsafe fn reset_errno() {
        unsafe { *libc::__errno_location() = 0 };
    }

    fn kind_of(mode: u32) -> EntryKind {
        match mode & libc::S_IFMT as u32 {
            m if m == libc::S_IFREG as u32 => EntryKind::File,
            m if m == libc::S_IFDIR as u32 => EntryKind::Dir,
            m if m == libc::S_IFLNK as u32 => EntryKind::Symlink,
            _ => EntryKind::Other,
        }
    }

    /// The kind a directory entry's `d_type` names, or `None` when the filesystem gave no type.
    pub(super) fn kind_from_dtype(d_type: u8) -> Option<EntryKind> {
        match d_type {
            libc::DT_REG => Some(EntryKind::File),
            libc::DT_DIR => Some(EntryKind::Dir),
            libc::DT_LNK => Some(EntryKind::Symlink),
            libc::DT_UNKNOWN => None,
            _ => Some(EntryKind::Other),
        }
    }

    fn fstat(file: &File) -> io::Result<libc::stat> {
        // SAFETY: zeroed stat is a valid out-parameter and fd is open.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(file.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    fn open_dir_at(base: RawFd, name: &CStr, shown: &Path) -> Result<File> {
        // SAFETY: base is an open directory fd (or AT_FDCWD) and name is NUL-terminated.
        let fd = unsafe {
            libc::openat(
                base,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let e = io::Error::last_os_error();
            if errno_is(&e, libc::ELOOP) {
                return Err(FsError::Symlink(shown.to_path_buf()));
            }
            if errno_is(&e, libc::ENOTDIR) {
                // A symlink to a directory reports ENOTDIR with O_DIRECTORY|O_NOFOLLOW on some
                // kernels; distinguish it from a plain non-directory.
                // SAFETY: as above, with a valid out-parameter.
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                let rc = unsafe {
                    libc::fstatat(base, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW)
                };
                if rc == 0 && kind_of(st.st_mode as u32) == EntryKind::Symlink {
                    return Err(FsError::Symlink(shown.to_path_buf()));
                }
                return Err(FsError::NotADirectory(shown.to_path_buf()));
            }
            return Err(FsError::io(shown, e));
        }
        // SAFETY: fd was just returned by openat and is owned here.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// `Ok(true)` when this call created the directory, `Ok(false)` when it already existed.
    ///
    /// macOS: a `parent` whose ACL would give the new directory an inheritable `allow` entry is refused
    /// before the `mkdirat` ([`FsError::UnsafeAcl`]), and a new directory that nevertheless
    /// carries an `allow` entry is (best effort) removed again, still empty, and refused. Nothing is
    /// repaired.
    fn mkdir_at(parent: &File, name: &CStr, shown: &Path) -> Result<bool> {
        let base = parent.as_raw_fd();
        crate::acl::guard_parent(parent, shown)?;
        // SAFETY: valid fd and C string.
        if unsafe { libc::mkdirat(base, name.as_ptr(), 0o700) } != 0 {
            let e = io::Error::last_os_error();
            if !errno_is(&e, libc::EEXIST) {
                return Err(FsError::io(shown, e));
            }
            return Ok(false);
        }
        #[cfg(feature = "testing")]
        super::testing::record_created(&name.to_string_lossy());
        if cfg!(target_os = "macos") {
            let checked = open_dir_at(base, name, shown)
                .and_then(|made| crate::acl::refuse_acl(&made, shown));
            if let Err(error) = checked {
                // SAFETY: valid fd and C string; AT_REMOVEDIR removes the empty directory just made.
                unsafe { libc::unlinkat(base, name.as_ptr(), libc::AT_REMOVEDIR) };
                return Err(error);
            }
        }
        Ok(true)
    }

    /// Flush a directory descriptor, reporting failure. The one place a directory is synced, so
    /// the `testing` seam sees every sync in order.
    fn sync_dir_file(dir: &File, policy: SyncPolicy) -> io::Result<Option<SyncLevel>> {
        #[cfg(feature = "testing")]
        if let Some(error) = super::testing::before_dir_sync(dir) {
            return Err(error);
        }
        super::flush(dir, policy).map(Some)
    }

    /// Makes the link to a directory this call has just created durable, by syncing `parent`.
    ///
    /// Called only when `mkdir_at` returned `Ok(true)` (the `mkdirat` succeeded; a directory that
    /// already existed, or that a racing creator made first, is `Ok(false)` and is neither synced
    /// nor looked at again). There is no record of earlier syncs and no walk over pre-existing
    /// directories. A failed sync is returned; the directory stays, and a later call finds it as
    /// an existing directory and does not sync it (the residual in the `atomic` module docs).
    fn publish_created(parent: &File, policy: SyncPolicy) -> io::Result<()> {
        sync_dir_file(parent, policy).map(|_| ())
    }

    /// `shared` skips the group/other-writable refusal (see `StateDir::open_shared`); the directory
    /// must still be a directory owned by the effective user.
    fn verify(file: &File, shown: &Path, shared: bool) -> Result<libc::stat> {
        let st = fstat(file).map_err(|e| FsError::io(shown, e))?;
        if kind_of(st.st_mode as u32) != EntryKind::Dir {
            return Err(FsError::NotADirectory(shown.to_path_buf()));
        }
        // SAFETY: geteuid has no preconditions.
        if st.st_uid != unsafe { libc::geteuid() } {
            return Err(FsError::NotOwned(shown.to_path_buf()));
        }
        if !shared && st.st_mode as u32 & 0o022 != 0 {
            return Err(FsError::TooPermissive(shown.to_path_buf()));
        }
        Ok(st)
    }

    impl Dir {
        fn fd(&self) -> RawFd {
            self.0.as_raw_fd()
        }

        pub fn pin(path: &Path, create: bool, options: &StateOptions, shared: bool) -> Result<Dir> {
            use std::path::Component;
            let root = CString::new("/").unwrap();
            let dot = CString::new(".").unwrap();
            let mut current = open_dir_at(
                libc::AT_FDCWD,
                if path.is_absolute() { &root } else { &dot },
                path,
            )?;
            let mut walked = std::path::PathBuf::new();
            for component in path.components() {
                let name = match component {
                    Component::RootDir => {
                        walked.push("/");
                        continue;
                    }
                    Component::CurDir => continue,
                    Component::ParentDir | Component::Prefix(_) => {
                        return Err(FsError::BadName(path.display().to_string()))
                    }
                    Component::Normal(n) => n,
                };
                walked.push(name);
                let c = CString::new(name.as_bytes())
                    .map_err(|_| FsError::BadName(path.display().to_string()))?;
                current = match open_dir_at(current.as_raw_fd(), &c, &walked) {
                    Err(FsError::Io { source, .. })
                        if create && errno_is(&source, libc::ENOENT) =>
                    {
                        let created = mkdir_at(&current, &c, &walked)?;
                        let next = open_dir_at(current.as_raw_fd(), &c, &walked)?;
                        // The new directory's entry lives in `current`. Publish it before
                        // descending, so a later durable write below cannot lose the link
                        // to the directory it went into. A failed flush stops here. Only a
                        // directory this call made is published; one that is already there
                        // (including one a racing creator made first) is left as it is.
                        if created {
                            publish_created(&current, options.sync).map_err(|source| {
                                FsError::DirSyncFailed {
                                    path: walked.clone(),
                                    source,
                                }
                            })?;
                        }
                        next
                    }
                    // Found, not made by this call: no sync, whoever made it and whenever.
                    Ok(next) => next,
                    Err(e) => return Err(e),
                };
            }
            let st = verify(&current, path, shared)?;
            // Defence in depth: the pinned inode must be the one the path names right now.
            match std::fs::symlink_metadata(path) {
                Ok(before)
                    if before.dev() == st.st_dev as u64 && before.ino() == st.st_ino as u64 => {}
                Ok(_) => return Err(FsError::IdentityChanged(path.to_path_buf())),
                Err(e) => return Err(FsError::io(path, e)),
            }
            Ok(Dir(current))
        }

        pub fn child(&self, name: &str, policy: SyncPolicy, shared: bool) -> Result<Dir> {
            let c = cname(name)?;
            let shown = Path::new(name);
            let mut created = false;
            let file = match open_dir_at(self.fd(), &c, shown) {
                Ok(f) => f,
                Err(FsError::Io { source, .. }) if errno_is(&source, libc::ENOENT) => {
                    created = mkdir_at(&self.0, &c, shown)?;
                    open_dir_at(self.fd(), &c, shown)?
                }
                Err(e) => return Err(e),
            };
            verify(&file, shown, shared)?;
            let child = Dir(file);
            // The entry lives in this directory; a child this call created is made durable like
            // a write. A child that already exists is returned as it is, with no sync.
            if created {
                publish_created(&self.0, policy).map_err(|source| FsError::DirSyncFailed {
                    path: shown.to_path_buf(),
                    source,
                })?;
            }
            Ok(child)
        }

        pub fn stat(&self, name: &str) -> Result<Option<EntryStat>> {
            let c = cname(name)?;
            // SAFETY: valid fd, C string, and zeroed out-parameter.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstatat(self.fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) }
                == 0
            {
                return Ok(Some(EntryStat {
                    kind: kind_of(st.st_mode as u32),
                    size: st.st_size as u64,
                    dev: st.st_dev as u64,
                    ino: st.st_ino as u64,
                    mode: st.st_mode as u32 & 0o7777,
                    modified_unix_ms: (st.st_mtime as u64) * 1000
                        + (st.st_mtime_nsec as u64) / 1_000_000,
                }));
            }
            let e = io::Error::last_os_error();
            if errno_is(&e, libc::ENOENT) || errno_is(&e, libc::ENOTDIR) {
                Ok(None)
            } else {
                Err(FsError::io(name, e))
            }
        }

        /// Windows sharing and access flags need no Unix open flags. `O_NONBLOCK` keeps a planted
        /// FIFO or device from hanging the open; the
        /// caller (`StateDir::open_read`) refuses a non-regular file and returns the file to
        /// blocking mode.
        pub fn open_read_with(
            &self,
            name: &str,
            _options: &crate::handle::ReadOptions,
        ) -> Result<File> {
            let c = cname(name)?;
            // SAFETY: valid fd and C string.
            let fd = unsafe {
                libc::openat(
                    self.fd(),
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
            };
            if fd < 0 {
                let e = io::Error::last_os_error();
                if errno_is(&e, libc::ELOOP) {
                    return Err(FsError::Symlink(name.into()));
                }
                return Err(FsError::io(name, e));
            }
            // SAFETY: fd just returned by openat.
            Ok(unsafe { File::from_raw_fd(fd) })
        }

        pub fn create_new(&self, name: &str, retain_name: bool) -> Result<File> {
            let c = cname(name)?;
            // macOS: no private file below a directory whose ACL would hand it an inheritable `allow` entry.
            crate::acl::guard_parent(&self.0, Path::new(name))?;
            // SAFETY: valid fd and C string; O_EXCL|O_NOFOLLOW means a planted symlink is never followed.
            let fd = unsafe {
                libc::openat(
                    self.fd(),
                    c.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600 as libc::c_uint,
                )
            };
            if fd < 0 {
                let e = io::Error::last_os_error();
                if errno_is(&e, libc::EEXIST) {
                    return Err(FsError::AlreadyExists(name.into()));
                }
                return Err(FsError::io(name, e));
            }
            // SAFETY: fd just returned by openat.
            let file = unsafe { File::from_raw_fd(fd) };
            #[cfg(feature = "testing")]
            if retain_name {
                super::testing::after_empty_create(&file)?;
            }
            // Temporary names may be removed while still empty. Direct permanent creation checks
            // this same handle in finish_read_open, once, without removing a visible final name.
            if !retain_name {
                if let Err(error) = crate::acl::refuse_acl(&file, Path::new(name)) {
                    drop(file);
                    // SAFETY: valid fd and C string.
                    unsafe { libc::unlinkat(self.fd(), c.as_ptr(), 0) };
                    return Err(error);
                }
            }
            Ok(file)
        }

        pub fn rename_replace(&self, from: &str, to: &str) -> Result<()> {
            let (cf, ct) = (cname(from)?, cname(to)?);
            // SAFETY: valid fds and C strings.
            if unsafe { libc::renameat(self.fd(), cf.as_ptr(), self.fd(), ct.as_ptr()) } != 0 {
                return Err(FsError::io(to, io::Error::last_os_error()));
            }
            Ok(())
        }

        /// No-replace publication: a hard link fails with EEXIST if `to` exists.
        pub fn link_new(&self, from: &str, to: &str) -> Result<()> {
            let (cf, ct) = (cname(from)?, cname(to)?);
            // SAFETY: valid fds and C strings.
            if unsafe { libc::linkat(self.fd(), cf.as_ptr(), self.fd(), ct.as_ptr(), 0) } != 0 {
                let e = io::Error::last_os_error();
                if errno_is(&e, libc::EEXIST) {
                    return Err(FsError::AlreadyExists(to.into()));
                }
                return Err(FsError::io(to, e));
            }
            Ok(())
        }

        pub fn unlink(&self, name: &str) -> Result<()> {
            let c = cname(name)?;
            // SAFETY: valid fd and C string.
            if unsafe { libc::unlinkat(self.fd(), c.as_ptr(), 0) } != 0 {
                return Err(FsError::io(name, io::Error::last_os_error()));
            }
            Ok(())
        }

        /// `unlinkat(AT_REMOVEDIR)`: removes an empty directory, and fails with `ENOTDIR` for a
        /// symlink or file at the name, so nothing but a real empty directory goes.
        pub fn remove_dir(&self, name: &str) -> Result<()> {
            let c = cname(name)?;
            // SAFETY: valid fd and C string.
            if unsafe { libc::unlinkat(self.fd(), c.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
                return Err(FsError::io(name, io::Error::last_os_error()));
            }
            Ok(())
        }

        /// Unlink `name` only when `fstatat(AT_SYMLINK_NOFOLLOW)` says it is a symlink. POSIX has no
        /// unlink-if-symlink primitive, so a name swapped for another kind between the check and the
        /// `unlinkat` is unlinked as that kind (a file or symlink name; a directory is refused by
        /// `unlinkat` without `AT_REMOVEDIR`); the link's target is never touched either way.
        pub fn remove_link(&self, name: &str) -> Result<()> {
            let c = cname(name)?;
            // SAFETY: valid fd, C string, zeroed out-parameter.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstatat(self.fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) }
                != 0
            {
                return Err(FsError::io(name, io::Error::last_os_error()));
            }
            if kind_of(st.st_mode as u32) != EntryKind::Symlink {
                return Err(FsError::io(
                    name,
                    io::Error::new(io::ErrorKind::InvalidInput, "the entry is not a symlink"),
                ));
            }
            self.unlink(name)
        }

        /// Remove `name` only after comparing the descriptor opened without following links to
        /// the name immediately before `unlinkat`. POSIX has no unlink-if-same-inode primitive,
        /// so an arbitrary mutator can still exchange the name after this comparison.
        pub fn unlink_regular(&self, name: &str) -> Result<bool> {
            let c = cname(name)?;
            // SAFETY: valid directory fd and NUL-terminated child name. O_NONBLOCK prevents a
            // planted FIFO from stalling cleanup before its type is rejected.
            let fd = unsafe {
                libc::openat(
                    self.fd(),
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let error = io::Error::last_os_error();
                return if errno_is(&error, libc::ENOENT) || errno_is(&error, libc::ELOOP) {
                    Ok(false)
                } else {
                    Err(FsError::io(name, error))
                };
            }
            // SAFETY: `fd` was just returned by openat and is owned by this File.
            let file = unsafe { File::from_raw_fd(fd) };
            let opened = fstat(&file).map_err(|error| FsError::io(name, error))?;
            if kind_of(opened.st_mode as u32) != EntryKind::File {
                return Ok(false);
            }
            // SAFETY: valid directory fd, child name, and stat out-parameter.
            let mut current: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::fstatat(
                    self.fd(),
                    c.as_ptr(),
                    &mut current,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                let error = io::Error::last_os_error();
                return if errno_is(&error, libc::ENOENT) {
                    Ok(false)
                } else {
                    Err(FsError::io(name, error))
                };
            }
            if kind_of(current.st_mode as u32) != EntryKind::File
                || opened.st_dev != current.st_dev
                || opened.st_ino != current.st_ino
            {
                return Ok(false);
            }
            // SAFETY: valid directory fd and child name. The comparison above narrows, but does
            // not eliminate, an unlink race with an arbitrary external path mutator.
            if unsafe { libc::unlinkat(self.fd(), c.as_ptr(), 0) } != 0 {
                let error = io::Error::last_os_error();
                return if errno_is(&error, libc::ENOENT) {
                    Ok(false)
                } else {
                    Err(FsError::io(name, error))
                };
            }
            Ok(true)
        }

        /// Flush this directory's entries. Unlike a file `fsync`, a failure is returned.
        pub fn sync(&self, policy: SyncPolicy) -> io::Result<Option<SyncLevel>> {
            sync_dir_file(&self.0, policy)
        }

        pub fn entries(&self, limit: usize) -> Result<Vec<DirEntry>> {
            // A fresh open file description of the pinned directory (`openat(dirfd, ".")`), not
            // a `dup`/`try_clone` of its descriptor: duplicated descriptors share one file
            // offset, so concurrent listings would consume each other's `getdents` position and
            // return partial results.
            let dot = CString::new(".").expect("no NUL in the string");
            let listing = open_dir_at(self.fd(), &dot, Path::new("."))?.into_raw_fd();
            // SAFETY: `listing` is a fresh owned fd; on success fdopendir takes ownership of it.
            let raw = unsafe { libc::fdopendir(listing) };
            if raw.is_null() {
                let e = io::Error::last_os_error();
                // SAFETY: fdopendir failed, so `listing` is still ours to close.
                unsafe { libc::close(listing) };
                return Err(FsError::io(".", e));
            }
            struct DirGuard(*mut libc::DIR);
            impl Drop for DirGuard {
                fn drop(&mut self) {
                    // SAFETY: pointer came from a successful fdopendir.
                    unsafe { libc::closedir(self.0) };
                }
            }
            // A new open file description starts at offset 0 and nothing else shares it.
            let guard = DirGuard(raw);
            let mut out = Vec::new();
            loop {
                // SAFETY: readdir signals end vs error through errno, which we clear first.
                unsafe { reset_errno() };
                let entry = unsafe { libc::readdir(guard.0) };
                if entry.is_null() {
                    let e = io::Error::last_os_error();
                    return if e.raw_os_error() == Some(0) {
                        Ok(out)
                    } else {
                        Err(FsError::io(".", e))
                    };
                }
                // SAFETY: d_name is a NUL-terminated array inside a live dirent.
                let raw_name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                let name = raw_name.to_string_lossy().into_owned();
                if name != "." && name != ".." {
                    if out.len() >= limit {
                        return Err(FsError::TooManyEntries(limit));
                    }
                    // SAFETY: d_type is a plain field of the live dirent.
                    let kind = match kind_from_dtype(unsafe { (*entry).d_type }) {
                        Some(kind) => kind,
                        None => {
                            #[cfg(feature = "testing")]
                            super::testing::count_fallback_stat();
                            // The filesystem reports no type: one descriptor-relative lstat.
                            // SAFETY: valid fd, NUL-terminated name, zeroed out-parameter.
                            let mut st: libc::stat = unsafe { std::mem::zeroed() };
                            let rc = unsafe {
                                libc::fstatat(
                                    self.fd(),
                                    raw_name.as_ptr(),
                                    &mut st,
                                    libc::AT_SYMLINK_NOFOLLOW,
                                )
                            };
                            if rc != 0 {
                                let e = io::Error::last_os_error();
                                if errno_is(&e, libc::ENOENT) {
                                    continue; // removed between readdir and fstatat
                                }
                                return Err(FsError::io(&name, e));
                            }
                            kind_of(st.st_mode as u32)
                        }
                    };
                    out.push(DirEntry { name, kind });
                }
            }
        }
    }
}

#[cfg(windows)]
#[path = "state/windows.rs"]
mod sys;

#[cfg(not(any(unix, windows)))]
mod sys {
    use super::{DirEntry, EntryKind, EntryStat, StateOptions};
    use crate::atomic::{SyncLevel, SyncPolicy};
    use crate::error::{FsError, Result};
    use std::fs::{self, File, OpenOptions};
    use std::path::{Path, PathBuf};

    #[derive(Debug)]
    pub struct Dir(PathBuf);

    fn is_link(meta: &fs::Metadata) -> bool {
        if meta.file_type().is_symlink() {
            return true;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return true; // FILE_ATTRIBUTE_REPARSE_POINT
            }
        }
        false
    }

    fn reject_links(path: &Path) -> Result<()> {
        for ancestor in path.ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(m) if is_link(&m) => return Err(FsError::Symlink(path.to_path_buf())),
                Ok(_) | Err(_) => {}
            }
        }
        Ok(())
    }

    impl Dir {
        pub fn pin(
            path: &Path,
            create: bool,
            _options: &StateOptions,
            _shared: bool,
        ) -> Result<Dir> {
            reject_links(path)?;
            if create {
                fs::create_dir_all(path).map_err(|e| FsError::io(path, e))?;
            }
            reject_links(path)?;
            if !fs::metadata(path)
                .map_err(|e| FsError::io(path, e))?
                .is_dir()
            {
                return Err(FsError::NotADirectory(path.to_path_buf()));
            }
            Ok(Dir(path.to_path_buf()))
        }
        pub fn child(&self, name: &str, _policy: SyncPolicy, shared: bool) -> Result<Dir> {
            let p = self.0.join(name);
            Dir::pin(&p, true, &StateOptions::default(), shared)
        }
        pub fn stat(&self, name: &str) -> Result<Option<EntryStat>> {
            let p = self.0.join(name);
            match fs::symlink_metadata(&p) {
                Ok(m) => Ok(Some(EntryStat {
                    kind: if is_link(&m) {
                        EntryKind::Symlink
                    } else if m.is_dir() {
                        EntryKind::Dir
                    } else if m.is_file() {
                        EntryKind::File
                    } else {
                        EntryKind::Other
                    },
                    size: m.len(),
                    dev: 0,
                    ino: 0,
                    mode: 0,
                    modified_unix_ms: m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                })),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(FsError::io(p, e)),
            }
        }
        pub fn open_read_with(
            &self,
            name: &str,
            _options: &crate::handle::ReadOptions,
        ) -> Result<File> {
            let p = self.0.join(name);
            if let Ok(m) = fs::symlink_metadata(&p) {
                if is_link(&m) {
                    return Err(FsError::Symlink(p));
                }
            }
            File::open(&p).map_err(|e| FsError::io(p, e))
        }
        pub fn create_new(&self, name: &str, _retain_name: bool) -> Result<File> {
            let p = self.0.join(name);
            let f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&p)
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        FsError::AlreadyExists(p.clone())
                    } else {
                        FsError::io(&p, e)
                    }
                })?;
            crate::private::restrict_owner_only(&p)?;
            Ok(f)
        }
        pub fn rename_replace(&self, from: &str, to: &str) -> Result<()> {
            fs::rename(self.0.join(from), self.0.join(to))
                .map_err(|e| FsError::io(self.0.join(to), e))
        }
        pub fn link_new(&self, from: &str, to: &str) -> Result<()> {
            fs::hard_link(self.0.join(from), self.0.join(to)).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    FsError::AlreadyExists(self.0.join(to))
                } else {
                    FsError::io(self.0.join(to), e)
                }
            })
        }
        pub fn unlink(&self, name: &str) -> Result<()> {
            fs::remove_file(self.0.join(name)).map_err(|e| FsError::io(self.0.join(name), e))
        }
        pub fn remove_dir(&self, name: &str) -> Result<()> {
            let p = self.0.join(name);
            match fs::symlink_metadata(&p) {
                Ok(m) if is_link(&m) => Err(FsError::Symlink(p)),
                Ok(_) => fs::remove_dir(&p).map_err(|e| FsError::io(p, e)),
                Err(e) => Err(FsError::io(p, e)),
            }
        }
        pub fn remove_link(&self, name: &str) -> Result<()> {
            let p = self.0.join(name);
            match fs::symlink_metadata(&p) {
                Ok(m) if is_link(&m) => fs::remove_file(&p)
                    .or_else(|_| fs::remove_dir(&p))
                    .map_err(|e| FsError::io(p, e)),
                Ok(_) => Err(FsError::io(
                    p,
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a link"),
                )),
                Err(e) => Err(FsError::io(p, e)),
            }
        }
        pub fn unlink_regular(&self, name: &str) -> Result<bool> {
            let path = self.0.join(name);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() => match fs::remove_file(&path) {
                    Ok(()) => Ok(true),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(FsError::io(path, error)),
                },
                Ok(_) | Err(_) => Ok(false),
            }
        }
        pub fn sync(&self, _policy: SyncPolicy) -> std::io::Result<Option<SyncLevel>> {
            Ok(None)
        }
        pub fn entries(&self, limit: usize) -> Result<Vec<DirEntry>> {
            let mut out = Vec::new();
            for entry in fs::read_dir(&self.0).map_err(|e| FsError::io(&self.0, e))? {
                let entry = entry.map_err(|e| FsError::io(&self.0, e))?;
                if out.len() >= limit {
                    return Err(FsError::TooManyEntries(limit));
                }
                let kind = match entry.file_type() {
                    Ok(t) if t.is_symlink() => EntryKind::Symlink,
                    Ok(t) if t.is_dir() => EntryKind::Dir,
                    Ok(t) if t.is_file() => EntryKind::File,
                    _ => EntryKind::Other,
                };
                out.push(DirEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    kind,
                });
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_attributes_map_to_kinds() {
        assert_eq!(kind_from_attributes(0x20), EntryKind::File); // ARCHIVE
        assert_eq!(kind_from_attributes(0x10), EntryKind::Dir);
        assert_eq!(kind_from_attributes(0x400), EntryKind::Symlink);
        // A directory symlink/junction is both: the reparse point wins.
        assert_eq!(kind_from_attributes(0x10 | 0x400), EntryKind::Symlink);
        assert_eq!(kind_from_attributes(0x80 | 0x2), EntryKind::File); // NORMAL | HIDDEN
    }

    #[cfg(unix)]
    #[test]
    fn dtype_maps_to_kinds_and_unknown_needs_a_stat() {
        use super::sys::kind_from_dtype;
        assert_eq!(kind_from_dtype(libc::DT_REG), Some(EntryKind::File));
        assert_eq!(kind_from_dtype(libc::DT_DIR), Some(EntryKind::Dir));
        assert_eq!(kind_from_dtype(libc::DT_LNK), Some(EntryKind::Symlink));
        for other in [libc::DT_FIFO, libc::DT_SOCK, libc::DT_CHR, libc::DT_BLK] {
            assert_eq!(kind_from_dtype(other), Some(EntryKind::Other));
        }
        assert_eq!(kind_from_dtype(libc::DT_UNKNOWN), None);
    }

    #[test]
    fn temp_prefix_is_bounded_and_cut_on_a_char_boundary() {
        assert_eq!(temp_prefix("abc"), "abc");
        assert_eq!(temp_prefix(&"a".repeat(255)).len(), 64);
        // 63 ASCII bytes then a 2-byte char straddling the 64-byte limit.
        let name = format!("{}\u{e9}tail", "a".repeat(63));
        assert_eq!(temp_prefix(&name).len(), 63);
        // The longest temp name still fits the platform component limit.
        let temp = format!(
            ".{}{TEMP_MARKER}{}-{}-{}",
            "a".repeat(64),
            u32::MAX,
            u64::MAX,
            31
        );
        assert!(temp.len() <= PLATFORM_NAME_LIMIT, "{}", temp.len());
    }

    #[test]
    fn name_checks_use_the_limit_they_are_given() {
        let long = "n".repeat(230);
        assert!(check_name(&long, PLATFORM_NAME_LIMIT).is_ok());
        #[cfg(not(windows))]
        assert!(check_name(&long, DEFAULT_WRITE_NAME_LEN).is_err());
        assert!(check_name(&"n".repeat(256), PLATFORM_NAME_LIMIT).is_err());
        assert!(check_name("a/b", PLATFORM_NAME_LIMIT).is_err());
    }
}

/// `open_shared`, `open_read`, `remove_dir` and `remove_link` (rightkit#613).
#[cfg(test)]
mod handle_policy_tests {
    use super::*;
    use crate::handle::ReadOptions;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        (dir, path)
    }

    #[test]
    fn open_read_returns_a_regular_file_handle() {
        let (_g, root) = tmp();
        let state = StateDir::create(&root.join("s")).unwrap();
        state.write("f", b"bytes").unwrap();
        let mut file = state.open_read("f", &ReadOptions::new()).unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut file, &mut text).unwrap();
        assert_eq!(text, "bytes");
        assert!(state
            .open_read("absent", &ReadOptions::new())
            .unwrap_err()
            .is_not_found());
        for bad in ["", "..", "a/b", "a\0b"] {
            assert!(matches!(
                state.open_read(bad, &ReadOptions::new()),
                Err(FsError::BadName(_))
            ));
        }
    }

    #[test]
    fn open_read_refuses_a_directory_and_can_require_a_single_link() {
        let (_g, root) = tmp();
        let state = StateDir::create(&root.join("s")).unwrap();
        state.child("sub").unwrap();
        assert!(matches!(
            state.open_read("sub", &ReadOptions::new()),
            Err(FsError::NotAFile(_) | FsError::Io { .. })
        ));
        state.write("f", b"x").unwrap();
        let strict = ReadOptions::new().require_single_link(true);
        assert!(state.open_read("f", &strict).is_ok());
        std::fs::hard_link(root.join("s/f"), root.join("s/alias")).unwrap();
        assert!(matches!(
            state.open_read("f", &strict),
            Err(FsError::HardLinked(_))
        ));
        assert!(state.open_read("f", &ReadOptions::new()).is_ok());
    }

    #[test]
    fn stat_reports_an_identity_that_tells_two_files_apart_and_a_hard_link_together() {
        let (_g, root) = tmp();
        let state = StateDir::create(&root.join("s")).unwrap();
        state.write("a", b"1").unwrap();
        state.write("b", b"2").unwrap();
        std::fs::hard_link(root.join("s/a"), root.join("s/a2")).unwrap();
        let (a, b, a2) = (
            state.stat("a").unwrap().unwrap(),
            state.stat("b").unwrap().unwrap(),
            state.stat("a2").unwrap().unwrap(),
        );
        assert!(a.ino != 0 && b.ino != 0, "an identity was reported");
        assert_ne!((a.dev, a.ino), (b.dev, b.ino));
        assert_eq!((a.dev, a.ino), (a2.dev, a2.ino));
    }

    #[test]
    fn remove_dir_removes_only_an_empty_real_directory() {
        let (_g, root) = tmp();
        let state = StateDir::create(&root.join("s")).unwrap();
        state.child("empty").unwrap();
        assert!(state.remove_dir("empty").unwrap());
        assert!(!state.exists("empty").unwrap());
        assert!(!state.remove_dir("empty").unwrap(), "missing is Ok(false)");
        // A directory with content stays, with its content.
        let full = state.child("full").unwrap();
        full.write("keep", b"x").unwrap();
        assert!(state.remove_dir("full").is_err());
        assert_eq!(full.read("keep", 10).unwrap(), b"x");
        // A file is not a directory.
        state.write("file", b"x").unwrap();
        assert!(state.remove_dir("file").is_err());
        assert!(state.exists("file").unwrap());
        assert!(matches!(state.remove_dir("../x"), Err(FsError::BadName(_))));
    }

    #[test]
    fn remove_link_refuses_content_that_is_not_a_link() {
        let (_g, root) = tmp();
        let state = StateDir::create(&root.join("s")).unwrap();
        state.write("file", b"x").unwrap();
        state.child("dir").unwrap();
        assert_eq!(
            state.remove_link("file").unwrap_err().kind(),
            Some(std::io::ErrorKind::InvalidInput)
        );
        assert_eq!(
            state.remove_link("dir").unwrap_err().kind(),
            Some(std::io::ErrorKind::InvalidInput)
        );
        assert!(state.exists("file").unwrap() && state.exists("dir").unwrap());
        assert!(!state.remove_link("absent").unwrap());
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::{symlink, PermissionsExt};

        #[test]
        fn open_shared_accepts_a_group_writable_directory_that_open_refuses() {
            let (_g, root) = tmp();
            let dir = root.join("shared");
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
            assert!(matches!(
                StateDir::open(&dir),
                Err(FsError::TooPermissive(_))
            ));
            let state = StateDir::open_shared(&dir).unwrap();
            state.write("f", b"x").unwrap();
            assert_eq!(state.read("f", 10).unwrap(), b"x");
        }

        #[test]
        fn open_shared_still_refuses_a_symlink_and_a_file() {
            let (_g, root) = tmp();
            let dir = root.join("real");
            std::fs::create_dir(&dir).unwrap();
            symlink(&dir, root.join("link")).unwrap();
            assert!(matches!(
                StateDir::open_shared(&root.join("link")),
                Err(FsError::Symlink(_))
            ));
            std::fs::write(root.join("file"), b"x").unwrap();
            assert!(matches!(
                StateDir::open_shared(&root.join("file")),
                Err(FsError::NotADirectory(_))
            ));
        }

        #[test]
        fn a_child_of_a_shared_directory_is_shared_and_of_a_private_one_is_not() {
            let (_g, root) = tmp();
            let dir = root.join("shared");
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
            let kid = dir.join("kid");
            std::fs::create_dir(&kid).unwrap();
            std::fs::set_permissions(&kid, std::fs::Permissions::from_mode(0o777)).unwrap();
            assert!(StateDir::open_shared(&dir).unwrap().child("kid").is_ok());
            let private = root.join("private");
            std::fs::create_dir(&private).unwrap();
            std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
            let pk = private.join("kid");
            std::fs::create_dir(&pk).unwrap();
            std::fs::set_permissions(&pk, std::fs::Permissions::from_mode(0o777)).unwrap();
            assert!(matches!(
                StateDir::open(&private).unwrap().child("kid"),
                Err(FsError::TooPermissive(_))
            ));
        }

        #[test]
        fn open_read_refuses_a_symlink_and_never_waits_on_a_fifo() {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            let (_g, root) = tmp();
            let state = StateDir::create(&root.join("s")).unwrap();
            state.write("real", b"x").unwrap();
            symlink(root.join("s/real"), root.join("s/link")).unwrap();
            assert!(matches!(
                state.open_read("link", &ReadOptions::new()),
                Err(FsError::Symlink(_))
            ));
            let fifo = root.join("s/pipe");
            let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
            // SAFETY: valid NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            assert!(matches!(
                state.open_read("pipe", &ReadOptions::new()),
                Err(FsError::NotAFile(_))
            ));
            assert!(matches!(state.read("pipe", 10), Err(FsError::NotAFile(_))));
        }

        #[test]
        fn remove_link_removes_the_link_and_never_the_target() {
            let (_g, root) = tmp();
            let state = StateDir::create(&root.join("s")).unwrap();
            let outside = root.join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("keep"), b"keep").unwrap();
            symlink(&outside, root.join("s/dirlink")).unwrap();
            symlink(root.join("nowhere"), root.join("s/dangling")).unwrap();
            assert!(state.remove_link("dirlink").unwrap());
            assert!(state.remove_link("dangling").unwrap());
            assert!(!state.exists("dirlink").unwrap());
            assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"keep");
            // remove_dir refuses a symlink to a directory and leaves it.
            symlink(&outside, root.join("s/again")).unwrap();
            assert!(state.remove_dir("again").is_err());
            assert!(state.exists("again").unwrap());
        }
    }

    #[cfg(windows)]
    mod windows {
        use super::*;

        /// A junction needs no privilege: `mklink /J`.
        fn junction(link: &Path, target: &Path) -> bool {
            // cmd.exe does not take a verbatim `\\?\` path.
            let plain = |p: &Path| crate::plain::plain_path(p.to_path_buf());
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(plain(link))
                .arg(plain(target))
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }

        #[test]
        fn a_junction_is_removed_by_remove_link_and_its_target_survives() {
            let (_g, root) = tmp();
            let state = StateDir::create(&root.join("s")).unwrap();
            let outside = root.join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("keep"), b"keep").unwrap();
            if !junction(&root.join("s").join("j"), &outside) {
                return; // cmd.exe unavailable in this environment
            }
            // The junction is refused everywhere a directory or file is expected.
            assert!(matches!(state.remove_dir("j"), Err(FsError::Symlink(_))));
            assert!(matches!(state.child("j"), Err(FsError::Symlink(_))));
            assert!(state.remove_link("j").unwrap());
            assert!(std::fs::symlink_metadata(root.join("s").join("j")).is_err());
            assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"keep");
        }

        #[test]
        fn read_only_sharing_blocks_a_writer_while_the_handle_is_open() {
            let (_g, root) = tmp();
            let state = StateDir::create(&root.join("s")).unwrap();
            state.write("f", b"x").unwrap();
            let shared = ReadOptions::new().share_read_only(true);
            let reader = state.open_read("f", &shared).unwrap();
            let writer = std::fs::OpenOptions::new()
                .write(true)
                .open(root.join("s").join("f"));
            assert_eq!(writer.unwrap_err().raw_os_error(), Some(32));
            drop(reader);
            assert!(std::fs::OpenOptions::new()
                .write(true)
                .open(root.join("s").join("f"))
                .is_ok());
        }
    }
}
