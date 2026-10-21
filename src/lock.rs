//! Cross-process advisory file locks.
//!
//! The lock is an OS advisory lock, so the OS releases it when the owner exits or is killed:
//! there is no TTL and no way for a live holder to be displaced. Owner details (pid, host,
//! label, start time) live in a sibling `<lock>.owner` file because Windows blocks other
//! processes from reading a locked file. An owner file whose lock can be acquired is stale
//! evidence of a crashed holder and is reported through [`FileLock::recovered_stale`].
//!
//! Unix takes one of two lock primitives, chosen per acquisition by [`LockKind`]:
//!
//! - [`LockKind::Fcntl`] (the default) is a POSIX record lock (`fcntl` `F_SETLK`). It belongs
//!   to the process, is not inherited across `fork`, and is dropped when the process closes
//!   *any* descriptor for the file, so the crate keeps one lock-holding descriptor per lock file
//!   per process and identifies the file by `(dev, ino)`, not by path: a hard link or a case alias
//!   of a held lock file is the same lock, not a second one.
//! - [`LockKind::Flock`] is a `flock(2)` lock, the primitive `std::fs::File::try_lock` uses on
//!   Unix. It belongs to the open file description, which a child forked before its `exec`
//!   inherits. The crate unlocks explicitly on drop, so an inherited descriptor cannot keep a
//!   released lock busy.
//!
//! Windows uses `LockFileEx` for both kinds, so the kind has no effect there.
//!
//! Do not rely on exclusion between the two kinds. A probe on macOS (Darwin 27, 0.3.4) found
//! that a `flock` held by one process refused an `fcntl` record lock in another process, and
//! the reverse. That is an observation, not a documented guarantee. Give one lock file one kind
//! for every user.
//!
//! The lock file itself is never deleted: removing it after unlock lets a second process lock
//! the old inode while a third creates and locks a new one for the same path.
//!
//! Owner records are JSON, so they need the `json` feature. Without it, `acquire` writes no
//! owner file, `recovered_stale` is always `None`, and `current_owner` and `probe_owner` return
//! `None`. The lock itself behaves the same either way. An owner record carries both
//! `started_unix_ms` and an RFC 3339 `started` field (the shape CutRight reads), and a read
//! accepts either one, so records written by older builds still parse.
//!
//! # The Unix registry: every request, what it does, what a crash leaves
//!
//! All rows run under one mutex, keyed by the opened file's `(dev, ino)`, never by a path.
//! "Entry" is the registry record of a held lock file (mode, holder count, every descriptor this
//! process has open on it). Tests are in `tests/lock_alias.rs` unless named otherwise.
//!
//! | Request (`LockKind::Fcntl`) | Registry state | Result | State left on a crash of this process | Test |
//! |---|---|---|---|---|
//! | exclusive or shared, file unheld | no entry for the inode | `open` (`0600`, `O_NOFOLLOW`), `F_SETLK`; granted: entry inserted, one holder; refused by another process: descriptor dropped, nothing recorded (this process holds nothing on it) | the kernel drops the record lock; the `.owner` file is stale evidence | `lock_cross_process.rs::exclusive_holder_in_another_process_blocks_every_mode`, `killed_holder_leaves_a_recoverable_lock` |
//! | shared | entry, shared | joins: holder count +1, no new lock call | as above | `lock_alias.rs::hard_link_shared_holders_keep_the_lock_until_the_last_drops` |
//! | exclusive | entry, any mode | busy: [`FsError::Locked`] / [`FsError::LockTimeout`]; no descriptor kept open beyond the entry's | n/a | `lock_alias.rs::a_hard_link_cannot_downgrade_or_release_an_exclusive_lock`, `lock_spawn.rs::shared_locks_coexist_and_exclude_exclusive` |
//! | shared | entry, exclusive | busy (a second `F_SETLK` from this process would silently downgrade the exclusive lock, so it is never issued) | n/a | same test |
//! | any, through an alias seen by the pre-open `lstat` (hard link, case alias, symlinked directory) | entry for that inode | answered from the entry, no `open` | n/a | `lock_alias.rs::a_case_alias_cannot_downgrade_or_release_an_exclusive_lock`, `a_symlinked_directory_reaches_the_same_lock`, `waiting_on_an_alias_does_not_accumulate_descriptors` |
//! | any, through a name swapped between the `lstat` and the `open` so the opened inode is held | entry for the opened inode | the new descriptor is parked in the entry and closed only with the last holder (closing it would drop the holder's lock); the request is answered by the entry | n/a | deterministic with the seam: `lock_alias.rs::swap::a_name_swapped_between_the_identity_check_and_the_open_never_lends_one_files_guard_to_another` (`lock::testing::after_identity_check`) |
//! | any, through a name swapped so the `lstat` identity (held) differs from what is opened | no remembered pairing | nothing is remembered between the two identities: a later request on the unchanged, unrelated inode takes its own real lock | n/a | same test (asserts a second process sees the unrelated inode locked) |
//! | release, not the last holder | holders > 1 | holder count -1; descriptors stay open | n/a | `lock_alias.rs::hard_link_shared_holders_keep_the_lock_until_the_last_drops` |
//! | release, last holder | holders = 1 | entry removed and every descriptor closed under the mutex, so no acquire in this process can open the file before the lock is gone | n/a | `lock_alias.rs::assert_alias_cannot_downgrade_or_release` (via `a_hard_link_...`) |
//! | `fork` while held (another thread spawns a child) | any | record locks are not inherited, so the child holds nothing | the child cannot keep the lock alive | `lock_spawn.rs::released_exclusive_lock_is_free_while_other_threads_spawn_children` |
//! | a panic while the mutex is held | any | poison is ignored; the guarded map is never left half-updated by a panic (every step is a single map operation) | n/a | cannot be tested without a panic injected between two map operations; there is no step between them to inject at |
//! | lock file missing | none | created `0600`; on macOS refused ([`FsError::UnsafeAcl`]) under a parent whose ACL would give it an inheritable `allow` entry, and a new file that still got an `allow` entry is refused and left in place (it may be another process's lock); deny-only ACLs pass | an empty `0600` lock file remains: it is never deleted | `tests/acl_macos.rs::a_lock_file_is_refused_under_an_inheriting_parent` |
//! | lock file is a symlink | n/a | [`FsError::Symlink`], nothing created | n/a | `lock_reparse.rs` |
//! | lock file named like a generated temp name | n/a | [`FsError::BadName`] | n/a | `lock_alias.rs::a_lock_file_named_like_a_generated_temp_file_is_refused` |
//!
//! [`LockKind::Flock`] keeps no registry: each attempt opens its own descriptor, the kernel
//! decides, and drop unlocks explicitly (`lock_spawn.rs::two_flock_descriptors_in_one_process_exclude_each_other`,
//! `lock_alias.rs::flock_kind_is_unaffected_by_aliases`, `lock_cross_process.rs::killed_flock_holder_releases_the_lock`).
//! Windows takes `LockFileEx` on one handle per attempt (no registry, not inherited); a reparse
//! point at the lock path is refused (`lock_reparse.rs`, Windows).
//!
//! Consolidates CutRight project/model-store/qa locks and CodeRight `DaemonDbLock`.

#[cfg(feature = "json")]
use crate::atomic::atomic_write;
use crate::error::{FsError, Result};
#[cfg(feature = "json")]
use serde::{Deserialize, Serialize};
#[cfg(not(unix))]
use std::fs::TryLockError;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Who holds a lock: recorded in `<lock>.owner` and read back by [`probe_owner`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "json", derive(Serialize, Deserialize))]
#[cfg_attr(
    feature = "json",
    serde(try_from = "OwnerRecord", into = "OwnerRecord")
)]
pub struct LockOwner {
    pub pid: u32,
    pub hostname: String,
    pub label: String,
    pub started_unix_ms: u64,
}

impl LockOwner {
    pub fn current(label: &str) -> Self {
        Self {
            pid: std::process::id(),
            hostname: hostname(),
            label: label.to_string(),
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }

    /// When the holder recorded it started.
    pub fn started(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.started_unix_ms)
    }

    pub fn describe(&self) -> String {
        format!(
            "pid {} on {} ({}) since unix-ms {}",
            self.pid, self.hostname, self.label, self.started_unix_ms
        )
    }
}

/// The owner file's JSON shape. Writes always carry both start-time fields. Reads prefer
/// `started_unix_ms` and fall back to RFC 3339 `started`, and a missing `label` reads as empty
/// (CutRight's records have none).
#[cfg(feature = "json")]
#[derive(Serialize, Deserialize)]
struct OwnerRecord {
    pid: u32,
    hostname: String,
    #[serde(default)]
    label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started: Option<String>,
}

#[cfg(feature = "json")]
impl TryFrom<OwnerRecord> for LockOwner {
    type Error = String;

    fn try_from(record: OwnerRecord) -> std::result::Result<Self, String> {
        let started_unix_ms = match (record.started_unix_ms, record.started.as_deref()) {
            (Some(ms), _) => ms,
            (None, Some(text)) => parse_rfc3339_ms(text)
                .ok_or_else(|| format!("start time {text:?} is not RFC 3339 UTC"))?,
            (None, None) => return Err("owner record has no start time".into()),
        };
        Ok(Self {
            pid: record.pid,
            hostname: record.hostname,
            label: record.label,
            started_unix_ms,
        })
    }
}

#[cfg(feature = "json")]
impl From<LockOwner> for OwnerRecord {
    fn from(owner: LockOwner) -> Self {
        Self {
            started: Some(format_rfc3339_secs(owner.started_unix_ms)),
            started_unix_ms: Some(owner.started_unix_ms),
            pid: owner.pid,
            hostname: owner.hostname,
            label: owner.label,
        }
    }
}

#[derive(Clone, Debug)]
pub struct LockOptions {
    /// How long to wait for a busy lock. `Duration::ZERO` tries exactly once.
    pub timeout: Duration,
    pub poll: Duration,
    /// Recorded in the owner file so a contender can say who holds the lock.
    pub label: String,
}

impl Default for LockOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::ZERO,
            poll: Duration::from_millis(50),
            label: String::new(),
        }
    }
}

impl LockOptions {
    pub fn wait(timeout: Duration) -> Self {
        Self {
            timeout,
            ..Self::default()
        }
    }

    pub fn labeled(mut self, label: &str) -> Self {
        self.label = label.to_string();
        self
    }
}

/// Which Unix lock primitive a lock uses. The default is [`LockKind::Fcntl`]. Windows ignores
/// the kind and takes `LockFileEx` for both.
///
/// - `Fcntl`: POSIX record lock. Per process, not inherited across `fork`. Closing any
///   descriptor for the file in this process releases it, so the crate shares one lock-holding
///   descriptor per lock file (by inode, however many names reach it) within a process.
/// - `Flock`: `flock(2)`. Per open file description. A child forked before its `exec` inherits
///   the description, so an unlock must be explicit (the crate does this on drop). Two
///   descriptors in one process exclude each other, which `Fcntl` does not do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LockKind {
    /// POSIX record lock (`fcntl` `F_SETLK`). The default.
    #[default]
    Fcntl,
    /// `flock(2)`, the primitive `std::fs::File::try_lock` uses on Unix.
    Flock,
}

/// Held until dropped (or the process dies).
#[derive(Debug)]
pub struct FileLock {
    /// Held for its `Drop`, which releases the lock; never read.
    #[cfg_attr(unix, allow(dead_code))]
    file: Handle,
    path: PathBuf,
    owner_path: Option<PathBuf>,
    /// Owner of a lock left behind by a process that is gone, when one was found.
    pub recovered_stale: Option<LockOwner>,
}

/// Which lock a caller wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Exclusive,
    Shared,
}

/// Unix: a process-held POSIX record lock or a `flock`, per the [`LockKind`] taken. Windows:
/// the locked file. The payloads are never read; each one releases its lock when dropped.
#[cfg(unix)]
#[derive(Debug)]
#[allow(dead_code)]
enum Handle {
    Fcntl(posix::Handle),
    Flock(flock::Handle),
}
#[cfg(not(unix))]
type Handle = File;

pub fn owner_path_for(lock_path: &Path) -> PathBuf {
    let mut os = lock_path.as_os_str().to_os_string();
    os.push(".owner");
    PathBuf::from(os)
}

/// Opens (creating if needed) the lock file itself, never what a link at that name points to.
/// Unix opens with `O_NOFOLLOW`; Windows opens with `FILE_FLAG_OPEN_REPARSE_POINT` and refuses a
/// reparse point (symlink, junction) by its attributes. Either way a link at `path` is
/// [`FsError::Symlink`]. Only the final component is checked; a symlinked parent directory is
/// followed, as for any path.
fn open_lock_file(path: &Path) -> Result<File> {
    if let Some(name) = path.file_name() {
        // A lock file named like a generated temp file would be deleted by a temp sweep.
        crate::atomic::reject_reserved_name(&name.to_string_lossy())?;
    }
    // A lock file this call creates is `0600` from creation. On macOS a parent whose ACL would
    // hand the new file an inheritable entry is refused first, and a new file that got an ACL
    // anyway is refused and left in place (see `crate::acl`): between the look and the open
    // another process can have created and locked this name, so the name is never unlinked
    // here. An existing lock file is opened as it is.
    #[cfg(unix)]
    let creating = matches!(
        std::fs::symlink_metadata(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    );
    #[cfg(unix)]
    if creating {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        crate::acl::guard_parent_path(parent, path)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|e| {
        #[cfg(unix)]
        if e.raw_os_error() == Some(libc::ELOOP) {
            return FsError::Symlink(path.to_path_buf());
        }
        FsError::io(path, e)
    })?;
    #[cfg(unix)]
    if creating {
        // A lock file that carries an `allow` entry anyway is refused. It is NOT removed: this
        // open is not exclusive, so the file may be one another process created and holds, and
        // unlinking it would let a third process lock a new file at the same name.
        crate::acl::refuse_acl(&file, path)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        let attributes = file
            .metadata()
            .map_err(|e| FsError::io(path, e))?
            .file_attributes();
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(FsError::Symlink(path.to_path_buf()));
        }
    }
    Ok(file)
}

impl FileLock {
    /// Exclusive lock, the first of the two lock primitives. Waits up to `options.timeout`
    /// (`Duration::ZERO` tries once), then records this process as owner in `<path>.owner`.
    /// Uses [`LockKind::Fcntl`]; see [`FileLock::acquire_kind`] to choose.
    /// Returns `FsError::Locked` (with the owner) or `FsError::LockTimeout` when busy.
    pub fn acquire(path: &Path, options: &LockOptions) -> Result<Self> {
        Self::acquire_kind(path, LockKind::Fcntl, options)
    }

    /// Exclusive lock with an explicit primitive. Waiting, owner recording and stale reporting
    /// are the same as [`FileLock::acquire`].
    pub fn acquire_kind(path: &Path, kind: LockKind, options: &LockOptions) -> Result<Self> {
        let file = lock_with_retry(path, options, kind, Mode::Exclusive)?;
        let recovered_stale = read_owner(&owner_path_for(path));
        let owner_path = record_owner(path, &options.label)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            owner_path,
            recovered_stale,
        })
    }

    /// Exclusive lock, failing immediately when busy. Shorthand for `acquire` with default options.
    pub fn try_acquire(path: &Path) -> Result<Self> {
        Self::acquire(path, &LockOptions::default())
    }

    /// Exclusive lock with the given primitive, waiting up to `timeout` (`Duration::ZERO`
    /// tries once). Records this process as owner with an empty label.
    pub fn try_lock_with(path: &Path, kind: LockKind, timeout: Duration) -> Result<Self> {
        Self::acquire_kind(path, kind, &LockOptions::wait(timeout))
    }

    /// Shared lock, the second primitive: any number of shared holders may coexist, and an
    /// exclusive holder excludes them all. Waits up to `options.timeout`. Writes no owner
    /// record, so `recovered_stale` is always `None`. Uses [`LockKind::Fcntl`].
    pub fn acquire_shared(path: &Path, options: &LockOptions) -> Result<Self> {
        Self::acquire_shared_kind(path, LockKind::Fcntl, options)
    }

    /// Shared lock with an explicit primitive. Behaves as [`FileLock::acquire_shared`].
    pub fn acquire_shared_kind(path: &Path, kind: LockKind, options: &LockOptions) -> Result<Self> {
        let file = lock_with_retry(path, options, kind, Mode::Shared)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            owner_path: None,
            recovered_stale: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The holder recorded on disk for `lock_path` (diagnostics; may be stale). Same as
    /// [`probe_owner`].
    pub fn current_owner(lock_path: &Path) -> Option<LockOwner> {
        probe_owner(lock_path)
    }
}

/// The owner record for `lock_path`, read without taking the lock. `None` when there is no
/// record, when it does not parse, or when the `json` feature is off. The record may name a
/// holder that has since exited.
pub fn probe_owner(lock_path: &Path) -> Option<LockOwner> {
    read_owner(&owner_path_for(lock_path))
}

/// Retries `mode` on `path` with `kind` until granted, timed out, or the lock file fails.
#[cfg(unix)]
fn lock_with_retry(
    path: &Path,
    options: &LockOptions,
    kind: LockKind,
    mode: Mode,
) -> Result<Handle> {
    let started = Instant::now();
    loop {
        let granted = match kind {
            LockKind::Fcntl => posix::try_lock(path, mode)?.map(Handle::Fcntl),
            LockKind::Flock => flock::try_lock(path, mode)?.map(Handle::Flock),
        };
        if let Some(handle) = granted {
            return Ok(handle);
        }
        wait_or_fail(path, options, started, mode)?;
    }
}

/// Windows: `LockFileEx` is per handle and not inherited, so one handle serves every attempt.
/// The kind has no effect here.
#[cfg(not(unix))]
fn lock_with_retry(
    path: &Path,
    options: &LockOptions,
    _kind: LockKind,
    mode: Mode,
) -> Result<Handle> {
    let file = open_lock_file(path)?;
    let started = Instant::now();
    loop {
        let locked = match mode {
            Mode::Exclusive => file.try_lock(),
            Mode::Shared => file.try_lock_shared(),
        };
        match locked {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => wait_or_fail(path, options, started, mode)?,
            Err(TryLockError::Error(e)) => return Err(FsError::io(path, e)),
        }
    }
}

/// Returns the timeout error once the wait budget is spent; otherwise sleeps one poll.
fn wait_or_fail(path: &Path, options: &LockOptions, started: Instant, mode: Mode) -> Result<()> {
    if started.elapsed() >= options.timeout {
        let waited_ms = started.elapsed().as_millis() as u64;
        return Err(match mode {
            Mode::Exclusive if options.timeout.is_zero() => {
                let owner = read_owner(&owner_path_for(path))
                    .map(|o| o.describe())
                    .unwrap_or_else(|| "an unknown owner".into());
                FsError::Locked {
                    path: path.to_path_buf(),
                    owner,
                }
            }
            _ => FsError::LockTimeout {
                path: path.to_path_buf(),
                waited_ms,
            },
        });
    }
    std::thread::sleep(options.poll);
    Ok(())
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Owner record goes first: while we still hold the lock nobody can have replaced it.
        if let Some(owner) = &self.owner_path {
            let _ = std::fs::remove_file(owner);
        }
        // Unix: dropping `self.file` releases the lock (see `posix` and `flock`).
        #[cfg(not(unix))]
        let _ = self.file.unlock();
    }
}

/// Unix locks are POSIX record locks, not `flock`.
///
/// `flock` belongs to the open file description. A child forked by another thread (for
/// example a `rightkit-child-process` spawn with `pre_exec`) inherits the descriptor and keeps the
/// lock alive until it execs. Record locks belong to the process and are not inherited across
/// `fork`.
///
/// Closing any descriptor for a file releases every record lock this process holds on it, and a
/// process's own record locks never conflict with each other (a second `F_SETLK` from the same
/// process replaces the first, so a shared request would silently downgrade an exclusive hold).
/// So a registry under one mutex records what this process holds, keyed by the file's identity
/// (`dev`, `ino`) and never by a spelling of its path: a hard link, a case alias on a
/// case-insensitive volume and a symlinked directory all reach the same entry. Acquire and
/// release both run under that mutex.
///
/// A descriptor that turns out to name an already-held file (an alias that the pre-open `lstat`
/// did not catch, which needs the file to be replaced in between) is parked in the entry and
/// closed only when the entry's last holder releases: closing it earlier, even when the
/// acquisition is refused, would drop the lock the real holder still needs.
#[cfg(unix)]
mod posix {
    use super::{open_lock_file, Mode};
    use crate::error::{FsError, Result};
    use std::collections::BTreeMap;
    use std::fs::{File, Metadata};
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// One lock file, named by the inode it is rather than by any path that reaches it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct FileId {
        dev: u64,
        ino: u64,
    }

    impl FileId {
        fn of(meta: &Metadata) -> Self {
            Self {
                dev: meta.dev(),
                ino: meta.ino(),
            }
        }
    }

    struct Entry {
        mode: Mode,
        holders: usize,
        /// Every descriptor this process holds open on the file. The first took the lock; the
        /// rest are alias descriptors kept open so that closing them cannot release it.
        files: Vec<File>,
    }

    /// Held locks by file identity. There is deliberately no map from "the identity a path's
    /// `lstat` reported" to "the identity of the file that was opened": the two can differ
    /// because the name was swapped between the calls, and remembering that pairing would later
    /// answer a lock request on the unrelated, unchanged inode by granting the other file's
    /// guard without taking any lock on it.
    #[derive(Default)]
    struct Registry {
        entries: BTreeMap<FileId, Entry>,
    }

    fn registry() -> MutexGuard<'static, Registry> {
        static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
        REGISTRY
            .get_or_init(|| Mutex::new(Registry::default()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// A record lock this process holds on one lock file. Released on drop.
    #[derive(Debug)]
    pub struct Handle {
        id: FileId,
    }

    /// Shared holders join a shared entry; anything else is busy. Decided here because this
    /// process's own record locks never conflict with each other.
    fn join(entry: &mut Entry, id: FileId, mode: Mode) -> Option<Handle> {
        if entry.mode == Mode::Shared && mode == Mode::Shared {
            entry.holders += 1;
            Some(Handle { id })
        } else {
            None
        }
    }

    /// Tries `mode` on the lock file at `path`. `Ok(None)` when busy, in this process or another.
    pub fn try_lock(path: &Path, mode: Mode) -> Result<Option<Handle>> {
        let mut registry = registry();

        // Identity first, without opening: an alias of a held file is answered with no new
        // descriptor. `lstat`, so a symlink at the name is never taken for the file it points to
        // (the open below refuses it).
        let seen = std::fs::symlink_metadata(path)
            .ok()
            .map(|meta| FileId::of(&meta));
        if let Some(seen) = seen {
            if let Some(entry) = registry.entries.get_mut(&seen) {
                return Ok(join(entry, seen, mode));
            }
        }

        #[cfg(feature = "testing")]
        super::testing::after_identity_check_run(path);
        let file = open_lock_file(path)?;
        let id = FileId::of(&file.metadata().map_err(|e| FsError::io(path, e))?);
        if let Some(entry) = registry.entries.get_mut(&id) {
            // Held already, reached through a name the `lstat` did not recognise. This
            // descriptor must outlive the acquisition whatever the answer: closing it would
            // release the lock that `entry` holds.
            entry.files.push(file);
            return Ok(join(entry, id, mode));
        }
        if !set_lock(&file, mode, path)? {
            // No entry names this file, so this process holds no record lock on it (its own
            // locks never make F_SETLK fail): closing this descriptor releases nothing.
            return Ok(None);
        }
        registry.entries.insert(
            id,
            Entry {
                mode,
                holders: 1,
                files: vec![file],
            },
        );
        Ok(Some(Handle { id }))
    }

    /// `Ok(true)` when granted; `Ok(false)` when another process holds a conflicting lock.
    fn set_lock(file: &File, mode: Mode, path: &Path) -> Result<bool> {
        match set_record_lock(file, lock_type(mode)) {
            Ok(granted) => Ok(granted),
            Err(error) => Err(FsError::io(path, error)),
        }
    }

    /// `F_WRLCK` / `F_RDLCK` as the `c_short` that `flock.l_type` holds. libc declares the two
    /// constants `c_short` on Apple targets and `c_int` on Linux, so the cast is a no-op on one
    /// and required on the other.
    #[allow(clippy::unnecessary_cast)]
    pub(super) fn lock_type(mode: Mode) -> libc::c_short {
        match mode {
            Mode::Exclusive => libc::F_WRLCK as libc::c_short,
            Mode::Shared => libc::F_RDLCK as libc::c_short,
        }
    }

    /// Takes a non-blocking POSIX record lock of `kind` (`F_WRLCK`/`F_RDLCK`) over the whole file.
    /// `Ok(false)` when another process holds a conflicting lock. Retries on `EINTR`.
    fn set_record_lock(file: &File, kind: libc::c_short) -> std::io::Result<bool> {
        use std::os::fd::AsRawFd;
        let request = libc::flock {
            l_type: kind,
            l_whence: libc::SEEK_SET as _,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        loop {
            // SAFETY: `file` is an open descriptor for the duration of the call, and `request` is
            // a fully initialised `flock` that outlives it; `F_SETLK` only reads through the
            // pointer.
            let rc = unsafe {
                libc::fcntl(
                    file.as_raw_fd(),
                    libc::F_SETLK,
                    &request as *const libc::flock,
                )
            };
            if rc != -1 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) | Some(libc::EACCES) => return Ok(false),
                _ => return Err(error),
            }
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            let mut registry = registry();
            let last = match registry.entries.get_mut(&self.id) {
                Some(entry) => {
                    entry.holders -= 1;
                    entry.holders == 0
                }
                None => false,
            };
            if last {
                // Closing the descriptors releases the lock. They close here, under the
                // mutex, so no acquire in this process can open the file before the lock is
                // gone.
                drop(registry.entries.remove(&self.id));
            }
        }
    }
}

/// `flock(2)` locks, for [`LockKind::Flock`].
///
/// Each attempt opens its own descriptor, so two `Flock` holders in one process exclude each
/// other, and `flock` does not need the per-process registry that `posix` uses. The lock is
/// per open file description, which a child forked before its `exec` inherits. Drop therefore
/// unlocks explicitly, so an inherited descriptor cannot keep a released lock busy.
#[cfg(unix)]
mod flock {
    use super::{open_lock_file, Mode};
    use crate::error::{FsError, Result};
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    /// A `flock` lock held through one open file description. Released on drop.
    #[derive(Debug)]
    pub struct Handle {
        file: File,
    }

    /// Tries `mode` on the lock file at `path`. `Ok(None)` when busy, in this process or another.
    pub fn try_lock(path: &Path, mode: Mode) -> Result<Option<Handle>> {
        let file = open_lock_file(path)?;
        let operation = match mode {
            Mode::Exclusive => libc::LOCK_EX,
            Mode::Shared => libc::LOCK_SH,
        };
        loop {
            // SAFETY: the descriptor is open for the duration of the call; `flock` takes no pointers.
            let rc = unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(Some(Handle { file }));
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EWOULDBLOCK) => return Ok(None),
                _ => return Err(FsError::io(path, error)),
            }
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the descriptor is still open here; `self.file` closes it after this body.
            // The explicit unlock releases the lock for every descriptor sharing this open file
            // description, including one a forked child has not yet exec'd away from.
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// Fault injection for consumer tests (feature `testing`, off by default; debug builds only).
///
/// [`after_identity_check`] runs a closure on the calling thread at the one point of a POSIX
/// record-lock acquisition where a name can be swapped under it: after the pre-open `lstat`
/// decided the file was not held, before the file is opened. It lets a test build the swap
/// deterministically.
#[cfg(feature = "testing")]
pub mod testing {
    use std::cell::RefCell;
    use std::path::Path;

    type Hook = Box<dyn Fn(&Path)>;

    thread_local! {
        static AFTER_IDENTITY_CHECK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Removes the hook when dropped.
    pub struct HookGuard(fn());

    impl Drop for HookGuard {
        fn drop(&mut self) {
            (self.0)();
        }
    }

    /// Run `hook(path)` on this thread between the identity check and the open of every
    /// `Fcntl` acquisition, until the guard drops.
    pub fn after_identity_check(hook: impl Fn(&Path) + 'static) -> HookGuard {
        AFTER_IDENTITY_CHECK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        HookGuard(|| AFTER_IDENTITY_CHECK.with(|slot| *slot.borrow_mut() = None))
    }

    #[cfg(unix)]
    pub(super) fn after_identity_check_run(path: &Path) {
        AFTER_IDENTITY_CHECK.with(|slot| {
            if let Some(hook) = slot.borrow().as_ref() {
                hook(path);
            }
        });
    }
}

/// Run `f` while holding the exclusive lock at `path`.
pub fn with_lock<T>(path: &Path, options: &LockOptions, f: impl FnOnce() -> T) -> Result<T> {
    let _guard = FileLock::acquire(path, options)?;
    Ok(f())
}

/// Writes the owner record for the exclusive holder of `lock_path`, returning its path.
#[cfg(feature = "json")]
fn record_owner(lock_path: &Path, label: &str) -> Result<Option<PathBuf>> {
    let owner_path = owner_path_for(lock_path);
    let record = serde_json::to_vec_pretty(&LockOwner::current(label)).expect("owner serializes");
    atomic_write(&owner_path, &record)?;
    Ok(Some(owner_path))
}

#[cfg(not(feature = "json"))]
fn record_owner(_lock_path: &Path, _label: &str) -> Result<Option<PathBuf>> {
    Ok(None)
}

#[cfg(feature = "json")]
fn read_owner(path: &Path) -> Option<LockOwner> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[cfg(not(feature = "json"))]
fn read_owner(_path: &Path) -> Option<LockOwner> {
    None
}

/// Days from 1970-01-01 to the given proleptic Gregorian date (Howard Hinnant's algorithm).
#[cfg(feature = "json")]
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = month as i64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian `(year, month, day)` for days since 1970-01-01.
#[cfg(feature = "json")]
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if month <= 2 {
        yoe + era * 400 + 1
    } else {
        yoe + era * 400
    };
    (year, month, day)
}

/// RFC 3339 UTC with whole seconds (`2020-01-01T00:00:00Z`), the form CutRight writes.
/// Sub-second precision is dropped here; `started_unix_ms` keeps it.
#[cfg(feature = "json")]
fn format_rfc3339_secs(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let second_of_day = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60
    )
}

/// Parses an RFC 3339 timestamp with a `T` or `t` separator, `Z` or a numeric offset, and an
/// optional fraction (first three digits kept). Returns milliseconds since the Unix epoch, or
/// `None` for anything out of range or before 1970.
#[cfg(feature = "json")]
fn parse_rfc3339_ms(text: &str) -> Option<u64> {
    // Byte slices below assume ASCII, so anything else is rejected before any slicing.
    if !text.is_ascii() {
        return None;
    }
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let number = |s: &str| -> Option<u64> {
        if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    let year = number(&text[0..4])? as i64;
    let month = number(&text[5..7])? as u32;
    let day = number(&text[8..10])? as u32;
    let hour = number(&text[11..13])?;
    let minute = number(&text[14..16])?;
    let second = number(&text[17..19])?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut index = 19;
    let mut millis = 0u64;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == start {
            return None;
        }
        let mut kept: String = text[start..index].chars().take(3).collect();
        while kept.len() < 3 {
            kept.push('0');
        }
        millis = kept.parse().ok()?;
    }

    let offset_secs: i64 = match bytes.get(index)? {
        b'Z' | b'z' if index + 1 == bytes.len() => 0,
        sign @ (b'+' | b'-') if bytes.len() == index + 6 && bytes[index + 3] == b':' => {
            let hours = number(&text[index + 1..index + 3])? as i64;
            let minutes = number(&text[index + 4..index + 6])? as i64;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let magnitude = hours * 3600 + minutes * 60;
            if *sign == b'+' {
                magnitude
            } else {
                -magnitude
            }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let utc_secs =
        days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64 - offset_secs;
    if utc_secs < 0 {
        return None;
    }
    Some(utc_secs as u64 * 1000 + millis)
}

#[cfg(feature = "json")]
fn days_in_month(year: i64, month: u32) -> u32 {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    }
}

pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: buffer is valid for `buf.len()` bytes and gethostname NUL-terminates within it.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Some(v) = std::env::var_os(var) {
            let s = v.to_string_lossy().trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "unknown".into()
}

#[cfg(all(test, unix))]
mod lock_type_tests {
    use super::{posix::lock_type, Mode};

    #[test]
    fn record_lock_types_survive_the_c_short_cast_and_stay_distinct() {
        // libc types the constants `c_short` (Apple) or `c_int` (Linux); `flock.l_type` is
        // `c_short` on both, so the cast must not change either value.
        assert_eq!(
            i64::from(lock_type(Mode::Exclusive)),
            i64::from(libc::F_WRLCK)
        );
        assert_eq!(i64::from(lock_type(Mode::Shared)), i64::from(libc::F_RDLCK));
        assert_ne!(lock_type(Mode::Exclusive), lock_type(Mode::Shared));
    }
}

#[cfg(all(test, feature = "json"))]
mod time_tests {
    use super::{days_from_civil, format_rfc3339_secs, parse_rfc3339_ms};

    #[test]
    fn known_dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(
            format_rfc3339_secs(1_577_836_800_000),
            "2020-01-01T00:00:00Z"
        );
        assert_eq!(
            parse_rfc3339_ms("2020-01-01T00:00:00Z"),
            Some(1_577_836_800_000)
        );
        // Leap day and end of year.
        assert_eq!(
            format_rfc3339_secs(parse_rfc3339_ms("2024-02-29T23:59:59Z").unwrap()),
            "2024-02-29T23:59:59Z"
        );
        assert_eq!(
            format_rfc3339_secs(parse_rfc3339_ms("1999-12-31T23:59:59Z").unwrap()),
            "1999-12-31T23:59:59Z"
        );
    }

    #[test]
    fn offsets_fractions_and_bad_input() {
        assert_eq!(
            parse_rfc3339_ms("2020-01-01T01:00:00+01:00"),
            parse_rfc3339_ms("2020-01-01T00:00:00Z")
        );
        assert_eq!(
            parse_rfc3339_ms("2020-01-01T00:00:00.5Z"),
            Some(1_577_836_800_500)
        );
        assert_eq!(
            parse_rfc3339_ms("2020-01-01t00:00:00.123456z"),
            Some(1_577_836_800_123)
        );
        assert_eq!(parse_rfc3339_ms("2020-02-30T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_ms("2019-02-29T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_ms("2020-01-01T00:00:00"), None);
        assert_eq!(parse_rfc3339_ms("2020-01-01 00:00:00Z"), None);
        assert_eq!(parse_rfc3339_ms("2020-01-0\u{e9}T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_ms("1969-12-31T23:59:59Z"), None);
        assert_eq!(parse_rfc3339_ms("not a time"), None);
    }
}
