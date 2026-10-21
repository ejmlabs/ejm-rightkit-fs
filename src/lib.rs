//! Safe filesystem state for Right Suite apps.
//!
//! - [`atomic`]: temp + `fsync` + rename replacement, streaming writer, crash-orphan sweep.
//!   [`atomic_write`] is the one-call entry point.
//! - [`lock`]: OS advisory cross-process locks with owner/stale reporting. Two primitives:
//!   [`FileLock::acquire`] / [`FileLock::try_acquire`] take the exclusive lock, and
//!   [`FileLock::acquire_shared`] takes a shared (reader) lock. [`LockKind`] picks POSIX record
//!   locks (`Fcntl`, the default) or `flock(2)` (`Flock`) on Unix; [`probe_owner`] reads the
//!   owner record without locking.
//! - [`lease`] (feature `fs`): [`Lease`], a pid + heartbeat-TTL lease file with stale-holder takeover
//!   (not an OS lock): `try_acquire`, `renew`, `release`, `peek`.
//! - [`migrate`] (feature `fs`): [`migrate_dir`] (also `AppDirs::migrate_dir`), a directory move that
//!   never shows a half-migrated destination (rename, or copy to a sibling then rename).
//! - [`app_dirs`] (feature `app-dirs`): [`AppDirs`], one resolver for an app's data, local, cache and
//!   home folders, with a dev namespace and test/QA isolation (QA overrides only with `test-mode`).
//! - [`digest`] (features `digest` / `blake3`): SHA-256 / BLAKE3 of bytes or files, & lowercase hex.
//! - [`plain`]: [`canonicalize_plain`], `std::fs::canonicalize` without the Windows verbatim drive prefix.
//! - [`contain`](mod@contain): [`is_within`], [`contain()`](contain::contain), [`contain_lenient`] and [`Roots`], path containment that
//!   follows links but refuses to leave a root; with feature `fs`, [`open_confined`] / [`Roots::open_confined`], a no-follow
//!   open that stays inside the approved folders.
//! - [`handle`] (feature `fs`): [`final_path`] and [`link_count`] of an open file, and [`open_read_nofollow`].
//! - [`names`]: [`sanitize_file_name`] for names that come from outside, [`numbered_candidates`], and with feature `fs`
//!   [`create_unique_file`] / [`create_download_file`].
//! - [`publish_dir`](mod@publish_dir) (feature `fs`): [`publish_dir()`](publish_dir::publish_dir), an atomic directory replace.
//! - [`trash`] (feature `fs`): [`move_to_trash`], and `trash::{list, lookup, restore}` (macOS and Windows).
//! - [`zip`]: [`StoredZipWriter`], a deterministic stored-method ZIP writer, and [`crc32`]; also
//!   `zip::{write_zip, read_zip, crc_table, crc32_update}` for archives built from files on disk.
//! - [`private`]: owner-only files and directories (0600/0700, Windows DACL).
//! - [`replaced`]: [`open_replaced`] / [`read_replaced`], readers that retry, on Windows, an open a concurrent
//!   replace refused (access denied, sharing violation).
//! - [`state`]: [`StateDir`], a descriptor-pinned state directory (`O_NOFOLLOW`, fd-relative
//!   `*at` operations) that a swapped or symlinked path cannot redirect.
//! - [`identity`] (feature `json`): stable file identity across renames and atomic replacement.
//! - [`clone`]: macOS `clonefile(2)` with `CLONE_NOFOLLOW`; [`clone_file_nofollow`] never falls
//!   back to a byte copy, [`clone_or_link_or_copy_nofollow`] tries clone, hard link, then copy.
//! - [`swap`]: macOS `renamex_np(RENAME_SWAP)`; atomically exchanges two paths ([`swap_paths`]).

#[cfg(all(feature = "testing", not(debug_assertions)))]
compile_error!(
    "rightkit-fs feature `testing` injects faults into atomic writes and is for debug and test builds only; \
     remove it from release builds"
);

#[cfg(all(feature = "fs", unix))]
pub(crate) mod acl;
#[cfg(feature = "app-dirs")]
pub mod app_dirs;
#[cfg(feature = "fs")]
pub mod append;
#[cfg(feature = "fs")]
pub mod atomic;
#[cfg(feature = "fs")]
pub mod clone;
pub mod contain;
#[cfg(any(feature = "digest", feature = "blake3"))]
pub mod digest;
#[cfg(feature = "fs")]
pub mod error;
#[cfg(feature = "fs")]
pub mod file_key;
#[cfg(feature = "fs")]
pub mod handle;
#[cfg(feature = "json")]
pub mod identity;
#[cfg(feature = "fs")]
pub mod lease;
#[cfg(feature = "lock")]
pub mod lock;
#[cfg(feature = "fs")]
pub mod migrate;
pub mod names;
#[cfg(feature = "fs")]
pub mod netfs;
pub mod plain;
#[cfg(feature = "fs")]
pub mod private;
#[cfg(feature = "fs")]
pub mod publish_dir;
#[cfg(feature = "fs")]
pub mod replaced;
#[cfg(feature = "fs")]
pub mod space;
#[cfg(feature = "fs")]
pub mod state;
#[cfg(feature = "fs")]
pub mod swap;
#[cfg(feature = "test-mode")]
pub mod test_mode;
#[cfg(feature = "fs")]
pub mod trash;
#[cfg(feature = "fs")]
pub mod tree;
#[cfg(all(feature = "fs", any(feature = "digest", feature = "blake3")))]
pub mod verified;
#[cfg(all(feature = "fs", windows))]
mod windows_temp_sweep;
pub mod zip;

#[cfg(feature = "app-dirs")]
pub use app_dirs::{AppDirs, AppDirsError, AppId, Options as AppDirsOptions, Os as AppDirsOs};
#[cfg(feature = "fs")]
pub use append::{append_record, open_private_append, AppendOptions};
#[cfg(all(feature = "fs", feature = "digest"))]
pub use atomic::atomic_write_with_digest;
#[cfg(feature = "fs")]
pub use atomic::{
    atomic_write, atomic_write_private, atomic_write_reporting, atomic_write_with,
    atomic_write_with_options, durable_sync, durable_sync_with, publication_durability,
    publish_file, publish_file_first_free, publish_file_with, rename_exclusive,
    rename_exclusive_reporting, sweep_stale_temps, sweep_stale_temps_report, try_sync_dir,
    AtomicOptions, AtomicWriteOptions, AtomicWriter, ExclusiveMethod, Privacy,
    PublicationDurability, ReplaceMode, RetryPolicy, SweepReport, SyncLevel, SyncPolicy,
    TempWriter, WriteReport,
};
#[cfg(feature = "fs")]
pub use clone::{
    clone_file_nofollow, clone_or_link_or_copy_nofollow, clone_or_link_or_copy_nofollow_with,
    CloneError, CloneOptions, CloneOutcome,
};
pub use contain::{contain, contain_lenient, is_within, ContainError, Roots};
#[cfg(feature = "fs")]
pub use contain::{open_confined, ConfinedOptions};
#[cfg(feature = "fs")]
pub use error::{FsError, PostPublishStep, Result};
#[cfg(feature = "fs")]
pub use file_key::FileKey;
#[cfg(feature = "fs")]
pub use handle::{final_path, link_count, open_read_nofollow, ReadOptions};
#[cfg(feature = "json")]
pub use identity::{IdentityStore, Resolution, StableId, Tracked};
#[cfg(feature = "fs")]
pub use lease::{Acquire, Lease, LeaseInfo, LeaseOptions, Staleness};
#[cfg(feature = "lock")]
pub use lock::{probe_owner, with_lock, FileLock, LockKind, LockOptions, LockOwner};
#[cfg(feature = "fs")]
pub use migrate::{migrate_dir, MigrateMethod, Migrated};
#[cfg(feature = "fs")]
pub use names::{create_download_file, create_unique_file};
pub use names::{
    is_windows_reserved_name, numbered_candidates, sanitize_file_name, sanitize_file_name_with,
    NameOptions,
};
#[cfg(feature = "fs")]
pub use netfs::is_network_filesystem;
pub use plain::{canonicalize_plain, plain_path};
#[cfg(feature = "fs")]
pub use private::{
    create_private_new, ensure_private_dir, restrict_owner_only, sweep_owner_only,
    write_private_executable, write_private_file, write_private_text,
};
#[cfg(feature = "fs")]
pub use publish_dir::{publish_dir, DirPublishMethod, PublishedDir};
#[cfg(feature = "fs")]
pub use replaced::{open_replaced, read_replaced, read_to_string_replaced};
#[cfg(feature = "fs")]
pub use space::free_bytes;
#[cfg(feature = "fs")]
pub use state::{DirEntry, EntryKind, EntryStat, StateDir, StateOptions};
#[cfg(feature = "fs")]
pub use swap::{is_swap_unsupported, swap_paths};
#[cfg(feature = "fs")]
pub use trash::{move_to_trash, TrashEntry, TrashIdentity};
#[cfg(feature = "fs")]
pub use tree::{copy_tree, CopyOptions};
#[cfg(all(feature = "fs", feature = "digest"))]
pub use verified::{copy_verified, copy_verified_with_progress, VerifiedCopy};
#[cfg(all(feature = "fs", any(feature = "digest", feature = "blake3")))]
pub use verified::{copy_verified_with_digest, DigestAlgorithm, VerifiedDigestCopy};
pub use zip::{crc32, stored_zip_bytes, StoredZipWriter};

/// Conservative liveness result used by crash recovery.
///
/// `Unknown` is deliberately distinct from `Alive`: permission and query failures do not prove
/// either state, and cleanup must never turn them into a deletion decision.
#[cfg(feature = "fs")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcessLiveness {
    Alive,
    Dead,
    Unknown,
}

/// Probe `pid` without treating a failed probe as proof of death.
#[cfg(feature = "fs")]
pub(crate) fn process_liveness(pid: u32) -> ProcessLiveness {
    #[cfg(unix)]
    {
        if pid == 0 || pid > libc::pid_t::MAX as u32 {
            return ProcessLiveness::Unknown;
        }
        // SAFETY: signal 0 performs only the existence/permission check.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            ProcessLiveness::Alive
        } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            ProcessLiveness::Dead
        } else {
            ProcessLiveness::Unknown
        }
    }
    #[cfg(windows)]
    {
        if pid == 0 {
            return ProcessLiveness::Unknown;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        // SAFETY: the PID is scalar; the exit-code pointer stays live through the call.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                // Error 87 alone is not proof that the process vanished. A complete bounded
                // process snapshot may prove absence; all other failed probes remain Unknown.
                if std::io::Error::last_os_error().raw_os_error() == Some(ERROR_INVALID_PARAMETER) {
                    return snapshot_liveness(pid);
                }
                return ProcessLiveness::Unknown;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            if ok == 0 {
                ProcessLiveness::Unknown
            } else if code == STILL_ACTIVE {
                ProcessLiveness::Alive
            } else {
                ProcessLiveness::Dead
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        ProcessLiveness::Unknown
    }
}

#[cfg(all(feature = "fs", windows))]
const ERROR_INVALID_PARAMETER: i32 = 87;
#[cfg(all(feature = "fs", windows))]
const ERROR_NO_MORE_FILES: i32 = 18;
#[cfg(all(feature = "fs", windows))]
const PROCESS_SNAPSHOT_LIMIT: usize = 4096;

#[cfg(all(feature = "fs", windows))]
#[repr(C)]
struct ProcessEntry32W {
    size: u32,
    usage: u32,
    process_id: u32,
    default_heap_id: usize,
    module_id: u32,
    threads: u32,
    parent_process_id: u32,
    priority_class: i32,
    flags: u32,
    executable: [u16; 260],
}

/// Return `Dead` only after a complete, bounded snapshot did not list `pid`.
#[cfg(all(feature = "fs", windows))]
fn snapshot_liveness_from_pids(pid: u32, pids: &[u32], complete: bool) -> ProcessLiveness {
    if complete && pids.len() < PROCESS_SNAPSHOT_LIMIT && !pids.contains(&pid) {
        ProcessLiveness::Dead
    } else {
        ProcessLiveness::Unknown
    }
}

/// A failed `OpenProcess` with ERROR_INVALID_PARAMETER still needs independent absence proof.
#[cfg(all(feature = "fs", windows))]
fn snapshot_liveness(pid: u32) -> ProcessLiveness {
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    // SAFETY: process ID zero selects every process; no pointers are dereferenced here.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot as isize == -1 {
        return ProcessLiveness::Unknown;
    }
    let mut pids = Vec::with_capacity(64);
    // SAFETY: zero initialization is valid for this C record after setting its required size.
    let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
    entry.size = std::mem::size_of::<ProcessEntry32W>() as u32;
    // SAFETY: snapshot is valid and `entry` remains valid for both enumeration calls.
    if unsafe { Process32FirstW(snapshot, &mut entry) } == 0 {
        // SAFETY: snapshot came from CreateToolhelp32Snapshot and is closed exactly once.
        unsafe { CloseHandle(snapshot) };
        return ProcessLiveness::Unknown;
    }
    let complete = loop {
        pids.push(entry.process_id);
        if pids.len() >= PROCESS_SNAPSHOT_LIMIT {
            break false;
        }
        entry.size = std::mem::size_of::<ProcessEntry32W>() as u32;
        // SAFETY: snapshot is valid and `entry` remains valid for the call.
        if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
            break std::io::Error::last_os_error().raw_os_error() == Some(ERROR_NO_MORE_FILES);
        }
    };
    // SAFETY: snapshot came from CreateToolhelp32Snapshot and is closed exactly once.
    unsafe { CloseHandle(snapshot) };
    snapshot_liveness_from_pids(pid, &pids, complete)
}

#[cfg(all(feature = "fs", windows))]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
    fn GetExitCodeProcess(handle: *mut std::ffi::c_void, code: *mut u32) -> i32;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut std::ffi::c_void;
    fn Process32FirstW(snapshot: *mut std::ffi::c_void, entry: *mut ProcessEntry32W) -> i32;
    fn Process32NextW(snapshot: *mut std::ffi::c_void, entry: *mut ProcessEntry32W) -> i32;
}

/// Whether process `pid` is not confirmed dead.
///
/// This remains a boolean compatibility helper. `true` includes an unavailable liveness probe;
/// callers that reclaim files must use the internal tri-state probe and require `Dead`.
#[cfg(feature = "fs")]
pub fn process_alive(pid: u32) -> bool {
    !matches!(process_liveness(pid), ProcessLiveness::Dead)
}

#[cfg(all(test, feature = "fs"))]
mod tests {
    use super::{process_alive, process_liveness, ProcessLiveness};

    #[test]
    fn current_process_is_alive() {
        assert_eq!(process_liveness(std::process::id()), ProcessLiveness::Alive);
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn invalid_pid_is_unknown_and_conservatively_alive() {
        assert_eq!(process_liveness(0), ProcessLiveness::Unknown);
        assert!(process_alive(0));
    }

    #[cfg(windows)]
    #[test]
    fn complete_snapshot_absence_confirms_death() {
        assert_eq!(
            super::snapshot_liveness_from_pids(42, &[1, 2, 3], true),
            ProcessLiveness::Dead
        );
    }

    #[cfg(windows)]
    #[test]
    fn snapshot_presence_is_unknown() {
        assert_eq!(
            super::snapshot_liveness_from_pids(42, &[1, 42, 3], true),
            ProcessLiveness::Unknown
        );
    }

    #[cfg(windows)]
    #[test]
    fn failed_snapshot_is_unknown() {
        assert_eq!(
            super::snapshot_liveness_from_pids(42, &[], false),
            ProcessLiveness::Unknown
        );
    }

    #[cfg(windows)]
    #[test]
    fn capped_snapshot_is_unknown() {
        assert_eq!(
            super::snapshot_liveness_from_pids(42, &[1; super::PROCESS_SNAPSHOT_LIMIT], true),
            ProcessLiveness::Unknown
        );
    }
}
