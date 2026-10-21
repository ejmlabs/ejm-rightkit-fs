# ejm-rightkit-fs

An ejmlabs-maintained fork of `rightkit-fs` 0.4.4, published under a distinct
package name. The Rust library remains `rightkit_fs`. Original MIT OR Apache-2.0
licenses and notices are retained.

This fork carries only the reviewed direct permanent-file creation and same-handle
privacy patch. Production source matches the harness's reviewed vendor tree.
It adds no dependencies or background service. See `FORK.md` for provenance,
verification limits and the upstream migration plan.

```toml
rightkit-fs = { package = "ejm-rightkit-fs", version = "=0.4.4-ejm.1", default-features = false, features = ["fs"] }
```

## Original upstream documentation

# rightkit-fs

Safe filesystem state for Right Suite apps:

- Descriptor-pinned state directories (`StateDir`). A swapped or symlinked path cannot redirect a pinned directory.
- Atomic writes (`atomic_write`, `atomic_write_private`, `atomic_write_with`, `atomic_write_with_options`, `AtomicWriter`): temp file, `fsync`, then rename. `AtomicWriteOptions` adds `ReplaceMode::{Rename, MoveWriteThrough, NoReplace}` (Windows `MoveFileExW` for the last two) and a `RetryPolicy` for sharing violations. There is no `ReplaceFileW` mode. A failure after the content is published is a typed error with `FsError::target_replaced()` true (`ReplacedButDirSyncFailed`, `ReplacedButPostStepFailed`). Temp files left by crashed writers are swept (`sweep_stale_temps`) only when the writer is confirmed dead (age never overrides a live or unqueryable PID), and only complete generated temp names are ever swept. What a durable write guarantees differs by platform: see [Atomic writes](#atomic-writes).
- Cross-process advisory locks (`FileLock`, `LockOptions`, `with_lock`), with owner and stale-owner reporting. `LockKind` chooses POSIX record locks (`Fcntl`, the default) or `flock(2)` (`Flock`) on Unix; `probe_owner` reads the owner record without locking.
- Owner-only files and directories (`write_private_file`, `ensure_private_dir`, `create_private_new`): mode 0600 and 0700 on Unix, a restricted DACL on Windows, and on macOS no extended ACL `allow` entry (see [Owner-only and macOS ACLs](#owner-only-and-macos-acls)).
- Stable file identity across renames and atomic replacement (`IdentityStore`, `StableId`; `Fingerprint` is `identity::Fingerprint`, not re-exported at the crate root; the root re-export list is at `src/lib.rs:65`).
- macOS `clonefile(2)` with `CLONE_NOFOLLOW` (`clone_file_nofollow`). It never falls back to a byte copy. `clone_or_link_or_copy_nofollow` is the explicit ladder for callers that can accept less: clone, then hard link, then a durable copy, reporting which one it was.
- macOS `renamex_np(RENAME_SWAP)` (`swap_paths`, `is_swap_unsupported`) and the `rightkit-swap A B` binary: atomic exchange of two paths, never absent in between.

Catalog capability: `safe-filesystem`. Published to crates.io; the registry record is 0.3.3 and the source is 0.4.0 (a breaking release, not yet published).

## When to use it

- Use it for any app state file that must not be torn by a crash, redirected by a symlink, or read by other users.
- Use it for cross-process locks on app-owned state.

Do not use it as a substitute for a database transaction. It guards single files and state directories.

## Cargo features

No feature is on by default (0.4.0). Name `fs`, `lock`, `json`, `digest`, `blake3` & `zip` as you use them (the 0.3.x default was all four); `testing` is never default.

- `blake3`: `digest::{Blake3Hasher, blake3_bytes, blake3_reader, blake3_file, blake3_file_with_len, blake3_file_with_progress}`. Depends only on `blake3`; does not enable SHA-256 or `fs`.
- `zip`: names the `zip` module (`zip::{write_zip, read_zip, crc_table, crc32_update}` beside `StoredZipWriter`). The module is compiled without any feature; `zip` is an empty alias that adds no code, no dependency and does not enable `fs`.
- `digest`: `digest` module (`sha256_bytes`, `sha256_reader`, `sha256_file`, `sha256_file_with_len`, `hex`). Depends only on `sha2`.
- `fs`: atomic writes, owner-only files, `StateDir`, netfs checks, clonefile, path swap (and the `rightkit-swap` binary), `FileKey` (`file_key`, no `json` needed), `free_bytes`, `copy_tree`, confined opens (`open_confined`), `handle`, `publish_dir`, `trash`, and the error type. Depends on `libc` (unix) and `thiserror`. Without any feature the crate still has `canonicalize_plain`, `contain` (`is_within`, `contain`, `Roots`), `names` (`sanitize_file_name`) and `zip` (`StoredZipWriter`).
- `lock`: cross-process locks (`FileLock`, `with_lock`). Depends on `fs` and `libc` (unix).
- `json`: identity ledger (`IdentityStore`), lock owner records, and the `rightkit-clonefile` binary. Depends on `lock`, `serde`, and `serde_json`.

Without `json`, locks write no owner file, so `recovered_stale` and `current_owner` return `None`. The lock itself behaves the same. `rightkit-db` uses `features = ["fs"]`. Digest-only callers use `features = ["digest"]`. Every integration test names the features it imports (`required-features`), so `--no-default-features` and each single feature compile.
- `testing` (debug builds only): fault and event seams for consumer tests: `atomic::testing`, `state::testing`, `clone::testing`, `lock::testing`. It implies `test-seams`.
- `test-seams` (allowed in release builds): `atomic::seams`, for a consumer's tests that run in the release profile. `fail_write_after(bytes, kind)` makes this thread's `AtomicWriter::write_all` / `AtomicWriter::writer()` writes fail after `bytes` bytes (the bytes up to the limit are written first); `sync_counts()`, `take_sync_counts()` and `reset_sync_counts()` count the file flushes and directory syncs this thread attempted. Per thread, inert until installed. It does not see a write made through `AtomicWriter::file_mut()` or the `File` an `atomic_write_with*` closure receives.

## State directories (`StateDir`)

`StateDir::open`/`create` use `StateOptions::default()`; `open_with`/`create_with` take explicit options. Children from `child` inherit them.

- **Permanent empty lock files.** `create_new_empty(name)` exclusively creates the final name relative to the pinned directory. It never publishes a second hard link or replaces an existing name. The returned write handle is checked for a regular file, exactly one hard link, and private ownership and access. Windows requires `Privacy::OwnerOnly`; inheritance settings and platforms other than Unix and Windows are refused before creation. No file or directory flush is performed, so this establishes process-lifetime identity, not persistence through power loss. For durable content use `write_new`.
- **Private reopening.** `ReadOptions::new().require_single_link(true).require_owner_only(true)` checks the opened file itself. Unix requires the effective user as owner, no group/other mode bits, and on macOS no ACL `allow` entry. Windows requires the crate's strict protected single-user DACL and adds `READ_CONTROL` to an opted-in pinned read. Default reads keep their existing checks and access. Checks are snapshots, not a promise against later changes. A post-create error leaves a permanent empty-file name in place: removing a visible lock could split its identity. Reopen through the same pinned directory with both options, and fail closed if either check fails. Neither an existing name nor one hard link proves private access. No check repairs permissions or removes an entry.

- **Durability.** `write`, `write_new`, `remove` and a creating `child` flush the file and then the directory, and a failed directory flush is an error (`ReplacedButDirSyncFailed` for writes, `DirSyncFailed` for removal and creation). `StateDir::create("new/deep")` and `child` publish the link of every directory **they create**: on Unix the *parent* of each is synced before descending (so a later durable write cannot lose a link this call made). A directory that already exists is not synced, and neither is one a racing creator made first; there is no record of earlier syncs and no walk over existing directories, so a call whose directories all exist makes no ancestor sync. A failed sync stops with `DirSyncFailed`. Atomic writes with `create_parents` and `durable` (and `atomic::create_dir_all_durable`, which `rightkit-app-settings` uses) follow the same rule. **Residual:** a directory created by an earlier call that died before its parent sync, or whose parent sync failed (`DirSyncFailed`, the directory stays), is not synced by later calls, however it got there, and a symlinked component is left alone with the ancestors of its target unwalked. That is still strictly more than 0.3.3, which synced no ancestor, but it is not a guarantee that every directory on the path has a durable link; a successful retry after `DirSyncFailed` is not proof of one. The module docs of `state` and `atomic` carry a table of every creation and publish path, what it syncs, what a crash leaves and which test covers it. **Windows** has no directory flush: `FlushFileBuffers` needs `GENERIC_WRITE`, and on the pinned list-access handle it fails with `ERROR_ACCESS_DENIED`. There, the file is flushed before publication, and creation (every directory `create` makes, including missing ancestors), the publishing rename and the delete are performed on `FILE_WRITE_THROUGH` handles. That asks the filesystem to commit each operation before the call returns; it is the strongest equivalent available, Microsoft documents no guarantee for directory metadata, and no power-loss test has been run.
- **Sync level.** On Apple targets the flush is `F_FULLFSYNC`, falling back to `F_BARRIERFSYNC` then `fsync` on volumes that refuse it. `durable_sync` keeps that silent fallback. `durable_sync_with(file, policy)` returns the `SyncLevel` reached (`Fsync < Barrier < Full`), and `SyncPolicy::Strict` (also `StateOptions::sync`) returns the `F_FULLFSYNC` error instead of falling back. `write_reporting` and `write_new_reporting` return the weakest level the file and directory flushes reached. Non-Apple targets always report `Full`, their strongest flush.
- **Names.** Reads, stats, removals and listings accept any name up to the platform limit (255 bytes on unix, 255 UTF-16 units on Windows), so an existing record longer than the write cap never fails a listing. Writes and `child` are held to `StateOptions::max_name_len` (default 200 bytes on unix, 255 units on Windows; clamped to the platform limit). A temp name carries at most 64 bytes of the real name, so a 255-byte name can be written. A write target shaped like a generated temp name (`.<name>.rktmp-<pid>-<n>-<attempt>`) is refused with `BadName`, because `sweep_temps` deletes such names; a name that merely contains `.rktmp-` is an ordinary record and is never swept.
- **Reads.** `read(name, max_bytes)` refuses a file over `max_bytes`; `u64::MAX` means no limit.
- **Listing.** Each listing opens its own directory description (`openat(dirfd, ".")` on Unix; Windows serialises enumeration), so concurrent listings from several threads each see every entry.
- **Listing.** `list` returns names. `list_entries` returns `DirEntry { name, kind }` from the directory record (`d_type` on unix, `FILE_ID_BOTH_DIR_INFO` attributes on Windows), with no stat per entry; a unix filesystem that reports no type costs one descriptor-relative `fstatat` for that entry. Windows reports any reparse point as `Symlink`.
- **`allow_rename_while_open` (Windows).** Default `false`: the pinned directory is opened without `FILE_SHARE_DELETE`, so no process can rename or delete it (or an ancestor) while the `StateDir` is open, and `path()` stays true. The 0.1.1 change that introduced handle-relative operations chose this on purpose (module docs: "without delete sharing"; the Windows-only test `rename_while_open_is_refused_by_default_and_allowed_by_the_option` in `src/state/windows_tests.rs`) and records no further reason. The git history attributes the handle-relative state change to the 0.1.1 release. The guarantee it gives: the pinned path cannot be renamed away and replaced (for example by a junction) underneath users of `path()`. `true` shares delete: the directory may be renamed or deleted under the open `StateDir`. Operations stay handle-relative and keep acting on the original directory, but `path()` then names whatever is there now, so use no path-based call on it. Unix descriptors always behave this way.
- **`privacy` (Windows ACL).** Default `Privacy::OwnerOnly`: directories and files the `StateDir` creates, including missing ancestors, get a protected owner-only DACL. If the `StateDir` is the first creator of an application data root, that root is protected and its inheritable ACE propagates to everything created under it later (other accounts, services and backup agents lose access). Either create the data root first, or pass `Privacy::OwnerOnlyUnixInheritWindows` (or `Inherit`): no ACL step, so new objects carry the parent's inherited ACL, as for atomic writes. Unix stays `0700`/`0600` regardless.

## Atomic writes

A write is two phases. Until the replace call succeeds nothing is published: a failure leaves the destination as it was and removes the temp file. After it, the content is in place; the required directory sync still runs, and a later failure is a typed error with `target_replaced()` true: `ReplacedButDirSyncFailed`, or `ReplacedButPostStepFailed { step }` (`TempCleanup`: a leftover second name after the `link` fallback). The most serious is reported: directory sync, then cleanup. `FsError` is `#[non_exhaustive]`. The `atomic` module docs hold a table of every publish path (`ReplaceMode` x `Privacy` x `durable` x platform x destination exists) with how the temp is created, what is synced when, which OS call publishes, what a crash at each step leaves, the error returned and the test that covers it (or the seam that stands in for the OS).

**Exclusive publish (`ReplaceMode::NoReplace`, `rename_exclusive`), per platform.** `rename_exclusive_reporting` (and `ExclusiveMethod`) says which mechanism ran.

| Platform and volume | Mechanism | Guarantee |
|---|---|---|
| Linux, Apple | `renameat2(RENAME_NOREPLACE)`, `renamex_np(RENAME_EXCL)` | One atomic create-new rename. The name is never visible empty. The loser gets `AlreadyExists`. |
| Unix without the exclusive rename, with hard links | `link(2)` then `unlink(2)` | Exclusive. Both names exist for an instant; a crash between leaves the temp for the sweep. |
| Unix without either (FAT, exFAT, some network shares: `link` fails with `EPERM`, `ENOTSUP`, `EMLINK`, `EINVAL` or `ENOSYS`) | An `O_EXCL` placeholder at the destination, then `rename(2)` of the finished file over it | Exclusive against every other exclusive publisher. **Not atomic for readers:** the name holds an empty file between the claim and the rename, and a crash there leaves that empty file. A non-exclusive replace can overwrite the placeholder. A failed rename removes the placeholder. Tested only through the `force_no_hard_links` seam; no FAT volume has been used. |
| Android | `link` then `unlink`, then the placeholder | As the two rows above. |
| Windows | `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` | Fails if the name exists; needs no hard links, so there is no fallback. Not exercised on FAT or a share by this crate. |

`publish_file_first_free(from, &candidates, options)` publishes one finished file under the first free name of a candidate list (for example `a.dng`, `a-1.dng`, ...) without rewriting it: each name is tried with the exclusive publish above, a taken name is skipped, the source is synced once, and the published path is returned. Every candidate taken is `AlreadyExists` for the last one.

To gate the content before it is published, use `AtomicWriter::commit_checked(|temp| ...)`: the check runs on the temp file just before the replace. The temp file is flushed first when the write is durable, and `commit_checked` always `sync_all`s it, in every mode, before the check. An error from the check aborts the write, leaves the destination as it was and removes the temp file.

What a durable write (`durable: true`, the default) guarantees when it returns `Ok`, by platform and mode. `publication_durability(&options, destination_exists)` returns the same answer as a value (`PublicationDurability`; `destination_exists` is accepted and ignored, no mode's answer depends on it):

| Platform and mode | Guarantee |
|---|---|
| Unix, any mode | The temp file is flushed (`F_FULLFSYNC` first on Apple), the replace runs, the parent directory is flushed: `DirectoryFlushed`. |
| Windows `MoveWriteThrough` and `NoReplace` | The temp file is flushed (`FlushFileBuffers`), then `MoveFileExW` with `MOVEFILE_WRITE_THROUGH`, which Microsoft documents as not returning until the move is on disk: `WriteThroughMove`. No power-loss test has been run. |
| Windows `Rename` (the default mode) | The temp file is flushed, then `std::fs::rename`. The rename is not write-through and there is no directory flush, so after a power loss the previous version can reappear (the file is never torn): `FileFlushedOnly`. |
| `durable: false`, any platform | Nothing is flushed: `None`. |

**Parent directories made by a durable `create_parents` write.** On Unix each directory this call creates has its link flushed (its parent `fsync`ed) before the next one. On Windows each directory this call creates is flushed with `FlushFileBuffers` on a `GENERIC_WRITE` handle opened with `FILE_FLAG_BACKUP_SEMANTICS` (a handle opened for listing cannot be flushed), and so is its parent, which holds its link, before the next component; so the parent chain is committed before the file is written below it. A failed flush is `DirSyncFailed` and the directory stays. A parent this call did not create that refuses the write handle with access denied (a caller that may only add children to it) is skipped. Only directories this call creates are flushed: one that already existed, or that a racing creator made first, is not looked at again, so a directory left by an earlier call that died before its flush is not repaired by later calls (the residual in the `atomic` module docs, section 3). What is proven: tests observe the flush calls and their order through the `testing` seam (`tests/atomic_publish.rs`, module `windows`). What is not: that the flush reaches the medium, which needs power-loss injection; no such test has been run. A symlinked component is left alone.

**Readers during a replace on Windows.** A replace never shows a reader a truncated file or a mix of two versions. On Windows it can refuse a reader for a moment: opening the destination while another process is replacing it can fail with access denied (error 5) or a sharing violation (error 32). Seen on the windows-2025 CI runner with three processes replacing one file in a loop (`tests/e2e.rs::concurrent_processes_never_expose_a_mixed_file`); the replace call there is `std::fs::rename`, the same call 0.3.3 used. Why the open is refused is not established: only the error code was observed. `open_replaced(path)`, `read_replaced(path)` and `read_to_string_replaced(path)` (feature `fs`) are the readers for a file another process replaces: on Windows they retry an open refused with error 5 or 32, up to 64 opens with a pause of 1 ms growing to 20 ms between them (a little over a second at worst), then return the last error unchanged, so a real permission failure still surfaces. Every other error, `NotFound` included, is returned at once. On other platforms they are one plain open, because `rename(2)` leaves an open or opening reader with the old or the new file. Only the open is retried; the bytes read are one complete version. The crate's other reads (`StateDir::read`, `read_to_string`) do not retry.

**Private writes on Windows.** `Privacy::OwnerOnly` (and `private: true`) creates the temp file with its owner-only security descriptor passed to `CreateFileW`, so it never exists with the parent's inherited ACL. Every publish moves the temp file, so its owner-only DACL is the published file's; the destination is never narrowed or inspected. Owner-only hardening of an existing path fails closed: there is no `icacls` fallback, because it would leave explicit ACEs for other principals in place. Unix creates the temp with mode `0600`.

**Names.** Public writes (and `rename_exclusive`, `create_private_new`, the clone ladder, lock files) refuse a target whose name is a complete generated temp name; `sweep_stale_temps` deletes only such names. On Windows the check also runs on the name the filesystem stores: Win32 drops leading spaces, trailing spaces and periods, and a `:stream` suffix, so `.record.rktmp-123-1-0 `, ` .record.rktmp-123-1-0` and `.record.rktmp-123-1-0:s` are refused too. (A name Windows would normalise to something that is *not* a temp name is accepted as it is. `swap_paths` does not call this check.)

## Append, verified copy, executables, and what a write achieved

- `append_record(path, bytes, AppendOptions)`: one `write` on an append-mode handle, then a flush (`durable`, default) that returns the `SyncLevel` reached; a file this call creates also has its directory flushed and can be created owner-only. No lock is taken and a failed append can leave a torn final record, so readers must tolerate one (torn-tail repair stays in the app). A symlink or a generated-temp name is refused. Log rotation belongs to `rightkit-logs`.
- `copy_verified(src, dest, options, expected_sha256)` (feature `digest`): copy to a temp beside `dest`, hash the bytes read, compare with the expected digest, re-hash the written temp, then publish atomically. Any mismatch (`FsError::DigestMismatch`) or a source that changed length leaves `dest` as it was and removes the temp.
- `write_private_executable(path, bytes)`: atomic owner-only write whose published file is `0700` on Unix (set on the temp descriptor before publication); Windows gets the owner-only DACL and is executable by file type.
- `AtomicWriter::commit_as(new_target, ReplaceMode)`: the same publish under a different final name in the writer's directory, for a name known only after the bytes are written (a content hash). `NoReplace` refuses an existing name.
- `try_sync_dir(dir)` (`atomic`): the fallible directory `fsync`; `sync_dir` remains the best-effort form.
- `open_private_append(path) -> File` (`append`): an append-mode handle for a caller's own records; a file it creates is owner-only, a symlink or a non-regular file is refused.
- `copy_tree(src, dst, CopyOptions { no_overwrite, keep_symlinks })` (`tree`): recursive copy that refuses to write over an existing entry when `no_overwrite`, and recreates symlinks when `keep_symlinks`. Not atomic: a failure leaves the entries already copied.
- `free_bytes(path) -> u64` (`space`): bytes available to the user on the volume holding `path`, from `statvfs` (unix) or `GetDiskFreeSpaceExW` (Windows); no child process.
- `digest::unhex(text)` and `digest::is_hex(text)`: decode hex (either case) and check hex text.
- `AppDirs::resolve_with_options(app, &Options)`: `resolve` with explicit options, so `dev(false)` gives the release namespace from a dev build.
- `FileKey::of_path` opens a directory on Windows too (`FILE_FLAG_BACKUP_SEMANTICS`); before, a directory failed with access denied.
- `AtomicWriter::commit_reporting()` and `atomic_write_reporting(...)` return a `WriteReport { sync_level, publication }`: the flush level the temp file reached (an Apple volume without `F_FULLFSYNC` shows `Barrier` or `Fsync`) and the `PublicationDurability` of the options. `is_fully_durable()` is true only for a `Full` flush with a directory flush or write-through publication.
- `sweep_stale_temps_report(dir)` returns a `SweepReport` that separates removed, live, unknown-liveness and other retained temps. On Windows only `removed` is counted (`retained_counted` is false).

## Containment, confined opens and handle checks

One module, `contain` (std only for the checks; the opens need `fs`), answers "is this path inside that folder" the same way everywhere: symlinks are followed with `canonicalize_plain`, a path that does not exist yet is resolved through its deepest existing ancestor, an existing entry that cannot be resolved (a dangling or looping link) is an error and never "absent", whole components are compared (never a string prefix), non-UTF-8 names are compared byte for byte, and case is ignored on Windows and macOS only.

- `is_within(root, path) -> bool`: absolute `path` is `root` or below it. A relative path, a `..` or a NUL byte in the missing part, an unresolvable entry and (Windows) an unusable new name (`NUL`, `COM1`, `a:stream`, a trailing dot or space) are all `false`.
- `contain(root, rel)` and `contain_lenient(root, rel)`: resolve a caller-supplied relative path under a root and refuse any escape (`ContainError::{InvalidRelative, Escapes, Io, ..}`). `rel` may hold no `..`, no root or drive or UNC prefix and no NUL; on Windows also no reserved device name (opening `root\NUL` opens the device, not a file below the root), no alternate-data-stream name and no trailing dot or space.
- `Roots` (`add`, `resolve_confined`, `resolve_confined_lenient`, `contains`): a set of approved folders, resolved when added.
- With `fs`: `open_confined(root, rel, &ConfinedOptions)` and `Roots::open_confined(path, &ConfinedOptions)` open **without following a link**. Unix opens the root with `O_NOFOLLOW`, then every component with `openat(O_NOFOLLOW)`, so no link can be followed at any step (a link anywhere is `ContainError::Symlink`, even one pointing inside); a `Roots` set also compares the opened root's device and inode with those recorded when it was approved, so a root swapped after approval is refused. The leaf is opened non-blocking and must be a regular file (`NotAFile`; a FIFO is never waited on). Windows checks each directory with `symlink_metadata`, opens the leaf with `FILE_FLAG_OPEN_REPARSE_POINT`, and rechecks the handle's final path (`GetFinalPathNameByHandleW`) below the root; that recheck runs after the open, so a creating or truncating open that a race redirected has already had its effect (it is reported, not undone). `ConfinedOptions::require_single_link(true)` refuses a file with more than one hard link (a hard link to a file outside the folder looks exactly like a file inside it); `share_read_only(true)` is the Windows read-sharing mode. A created file is `0600` on Unix.
- `handle` (`fs`): `final_path(&File)` (macOS `F_GETPATH`, Linux `/proc/self/fd`, Windows `GetFinalPathNameByHandleW`), `link_count(&File)`, `open_read_nofollow(path, &ReadOptions)` (no link at the final component, regular files only, optional single-link and Windows read-only sharing; **parents are followed**, use `StateDir` or `Roots::open_confined` to pin them). `FileKey::of_file` is the identity of any handle.
- `StateDir` (rightkit#613): `open_read(name, &ReadOptions)` is the public no-follow handle open; `stat` reports a real volume serial and file index on Windows (`dev`/`ino` were 0); `open_shared(path)` pins a directory that is **not** private (group- or world-writable accepted, symlinks and non-directories still refused, owner still checked on Unix; the caller owns the consequences, see its docs); `remove_dir(name)` removes an empty real directory and refuses a link; `remove_link(name)` removes a symlink or junction itself, never its target, and refuses anything that is not a link (Windows: the reparse point is opened and deleted through that handle).
- `private::is_owner_only` on Windows is a verify-only trust check that **accepts an inherited owner-only ACL**: the owner is the token user (or an enabled owner-capable group), and every `allow` ACE, explicit or inherited, names the current user, `SYSTEM`, `Administrators` (or an inherit-only `CREATOR OWNER`). Any `allow` for another user, `Everyone`, `Users`, `Authenticated Users` or anything unreadable or unknown is `false`; `deny` ACEs are ignored. A runtime directory below `%LOCALAPPDATA%` therefore verifies without being rewritten (rightkit#676, #517). `is_owner_only_strict` is the exact protected single-ACE form this crate writes (the form every object it creates is verified against); `verify_owner_only(path)` is the checked call returning `NotOwned`, `TooPermissive` or `Symlink`.

## File names from outside, directory publish, stored ZIP, trash

- `sanitize_file_name(raw)` (no feature): one safe, portable component from a download or attachment name. Keeps the last segment after `/` or `\`; removes controls, invisible and bidirectional-override characters; replaces `: * ? " < > |`; drops leading and trailing dots and spaces; prefixes `_` to a reserved Windows device name; cuts to 200 bytes keeping the extension; never empty, never `.` or `..`. `numbered_candidates(name, n)` lists `name`, `name (1)`, ...; with `fs`, `create_unique_file(dir, name)` creates the first free one exclusively (`create_new`, a symlink at the name is skipped, never followed) and `create_download_file(dir, raw)` sanitises first.
- `publish_dir(src, dest)` (`fs`): atomic directory replace. `dest` absent: exclusive rename. `dest` a directory: macOS `RENAME_SWAP` or Linux `RENAME_EXCHANGE` (atomic), else the old directory is moved aside, the new one published exclusively and the old tree removed (a crash in that window leaves the old tree under `.<name>.rkold-...` and no `dest`; a failed publish moves it back). `dest` a file or symlink is refused. The old tree is removed only after the new one is in place; a removal failure is `ReplacedButPostStepFailed { step: OldTreeCleanup }`. It does not flush files inside `src`.
- `StoredZipWriter` / `stored_zip_bytes` / `crc32` (no feature): a deterministic stored-method ZIP (fixed DOS timestamp, no extra fields, Unix `0644`/`0755`, entries in the order added, UTF-8 names), no compression and no ZIP64 (over the limits is an error). Names are checked: no absolute or drive-prefixed names, backslashes, `.`/`..`/empty components, control characters, or duplicates.
- `trash` (`fs`; macOS and Windows): `move_to_trash(path) -> TrashIdentity`, `trash::{list, lookup, restore}`. macOS uses `FSMoveObjectToTrashSync`; Windows `move_to_trash` is refused with `FsError::Unavailable` before anything is touched (`SHFileOperationW` deletes permanently when an item cannot be recycled and no recycle-only operation is bound); `list`/`lookup`/`restore` read the `$Recycle.Bin` `$I`/`$R` records. The identity is the `FileKey`, the original path and the trashing time; restore is an exclusive rename and never overwrites. Windows record reads cover drive-lettered fixed drives only (see the module docs). Other platforms return `FsError::Unavailable`.

## Owner-only and macOS ACLs

`0600`/`0700` constrain the BSD mode bits only. macOS evaluates ACL `allow` entries before them, and a new file or directory inherits its parent's inheritable entries. So on macOS every private creation (`Privacy::OwnerOnly`, `OwnerOnlyUnixInheritWindows`, `private: true`, `create_private_new`, `ensure_private_dir`, `StateDir` files and directories, lock files) first reads the parent's ACL and, if it holds an inheritable `allow` entry, refuses with `FsError::UnsafeAcl` and creates nothing. A new entry that carries an `allow` entry anyway (the parent's ACL changed between the read and the creation) is removed while still empty, before any byte is written, and refused with the same error. The one exception is a lock file: its open is not exclusive, so the file may be one another process created and holds, and it is refused and left in place, never unlinked. One predicate decides all of it: an ACL is unsafe iff it has an `allow` entry. A `deny` entry cannot widen access, so a deny-only ACL (on the parent, on a new entry, or on an existing one) passes everywhere; an entry whose type cannot be read counts as `allow`. **A path taken for a directory is opened with `O_DIRECTORY`**, so a regular file passed by mistake is refused without being opened. Functions that read a file (`StateDir::read`, the digests, `FileKey::of_path`, `Fingerprint::of_path`, the clone ladder's source) do open and close it: never call them on a file this process holds a POSIX record lock on. **An entry that already exists is never opened to read its ACL**: `restrict_owner_only` and `private::is_owner_only` read it by path (`acl_get_link_np`), because closing a descriptor releases every POSIX record lock the process holds on that file, SQLite's on a live database and its `-wal`/`-shm` included (`tests/private_keeps_locks.rs`). **No ACL is ever removed from an inode**: that cannot revoke access through a descriptor somebody already holds open, so a "repair" would report success while the exposure stood. `restrict_owner_only` narrows an existing entry's mode and then refuses an entry that has an `allow` entry (`UnsafeAcl`, the ACL left as it is), and `private::is_owner_only` is false for an entry with one. (Windows: `is_owner_only` accepts an inherited owner-only ACL and rejects any `allow` ACE for another principal; see the containment section.) Non-inheritable entries on a parent are accepted: the new entry does not receive them. Linux masks default-ACL entries with the creation mode, so a `0600` creation grants nothing extra; other ACL models are not handled. `clone_file_nofollow` copies the source's mode and receives the destination directory's inheritable ACL entries; it is not an owner-only creation. The macOS ACL calls are hand-declared against `<sys/acl.h>`, and the source records that none of them has been compiled or run in this tree (`src/acl.rs`, module docs).

## Clone, link, copy

`clone_or_link_or_copy_nofollow(src, dst)` (and `_with(src, dst, &CloneOptions)`) creates `dst` from the regular file `src` by the first step that works and returns which one: `CloneOutcome::Cloned` (macOS `clonefile`, Linux `FICLONE` where the filesystem has reflinks; Windows has no clone step), `HardLinked` (same volume), or `Copied` (a durable `NoReplace` atomic write; Unix permission bits are copied). **On macOS a source with no group or other permission bit is never cloned natively**: `clonefile` would create the destination with the source's mode and the destination directory's inheritable ACL, so owner-only cannot be established; such a source goes to the hard link, then to the copy, which creates its destination `0600` first. A source readable by others is cloned as before. A link at `src` or `dst` is refused (`FsError::Symlink`), never followed; an existing `dst` is `FsError::AlreadyExists` and never overwritten. **A hard link shares content with the source**: an in-place write through either name changes both, and deleting the destination removes only a name, so a caller that tracks copies should treat `HardLinked` ones as one item. Set `CloneOptions::allow_hard_link = false` (or `.without_hard_link()`) when the copy will be modified in place. Set `CloneOptions::fresh_metadata` (or `.fresh_metadata(true)`) to make an export that does not inherit the source's mode or extended attributes: the clone and link steps are skipped and the result is always `Copied`, with the default mode for a new file (honouring the umask).

## Locks

- `LockKind::Fcntl` (default) is a POSIX record lock; `LockKind::Flock` is `flock(2)`. Both are public (`FileLock::acquire_kind`, `acquire_shared_kind`, `try_lock_with`) and tested (`tests/lock_spawn.rs`, `tests/lock_cross_process.rs`). Windows uses `LockFileEx` for both. **Fcntl and flock do not promise to exclude each other.** The source records a probe on macOS (Darwin 27) that saw each refuse the other across processes (`src/lock.rs:14-18`); no test in the crate covers it, it is an observation, not a guarantee, and other kernels differ (Linux treats them as independent locks). Give one lock file one kind for every user.
- **One file is one lock, whatever it is called.** The Unix `Fcntl` registry is keyed by the opened file's `(dev, ino)`, not by a path: a hard link, a case alias on a case-insensitive volume and a symlinked directory all reach the same entry. POSIX record locks belong to the process and the file, so a second descriptor would neither conflict with the first nor be safe to close; the crate keeps any alias descriptor open until the entry's last holder releases, so a refused or released alias never drops a lock someone still holds. (`tests/lock_alias.rs`.)
- The lock file is opened without following a link at its name: `O_NOFOLLOW` on unix, `FILE_FLAG_OPEN_REPARSE_POINT` plus a reparse-attribute check on Windows. A link there is `FsError::Symlink`. Only the final component is checked.
- **Migrating from a create-new marker lock.** Not implemented as an API; the guidance is below. A consumer whose old versions take a lock by creating a marker file (`O_EXCL`/`create_new`, removed on release) cannot simply point `FileLock` at the same path during an update overlap. The new lock never deletes its file and holds no marker semantics, so the old version's staleness rule would see a permanent file and could delete it, and the old version would not see the new lock as held. To migrate:
  1. Give `FileLock` its own path (for example `<name>.flock`), never the marker path.
  2. For as long as old versions can run, take the `FileLock` first (it orders new versions among themselves), then create the old marker with the old format (`create_private_new`), treating `AlreadyExists` by the old version's own staleness rule, and remove the marker before releasing the `FileLock`. Old versions then see the marker; new versions serialize on the lock.
  3. A crash between the two steps leaves a marker only the old staleness rule can clear, exactly as before; the new lock needs no cleanup.
  4. Once old versions are gone, delete the marker code and the marker path.

  The marker format and staleness rule belong to the consumer, so a generic mode would need both as parameters and would still leave orphaned markers to the consumer's policy; that is why this is guidance, not a mode.
- `Lease` (`fs`): a pid + heartbeat-TTL lease file, not an OS lock. `Lease::try_acquire(path, &LeaseOptions::new(ttl).label("..."))` creates the record exclusively and returns `Acquire::Acquired(lease)`, or `Acquire::Held(info)` for a live holder; `lease.renew()` heartbeats (call it every `ttl / 3` or sooner; `FsError::Locked` or `NotFound` means the lease was lost); `lease.release()` or drop removes the record only if it is still this lease's; `Lease::peek(path)` reads it. A record is stale when its pid is confirmed dead or its last heartbeat is older than its TTL; a stale record is taken over, a live and fresh one never. Local machine only, wall-clock based, never sleeps. See `src/lease.rs` for the takeover protocol and its stated races.
- `migrate_dir(from, to)` / `AppDirs::migrate_dir(from, to)` (`fs`): move a directory so `to` is never half-migrated. Same volume: one rename. Different volumes: copy to a hidden sibling of `to`, rename it to `to`, remove `from`. `to` must be absent or empty (a populated `to` is `FsError::AlreadyExists`, nothing written). Safe to repeat after an interruption; stale hidden copies of dead processes are swept.

## Platform support

- Unix: file operations are descriptor-relative, with `O_NOFOLLOW` and `*at` calls.
- Windows: operations are handle-relative. Reparse points and symlinks are refused (`FsError` reports them).
- macOS: `clone_file_nofollow` uses `clonefile(2)`; `swap_paths` uses `renamex_np(RENAME_SWAP)` (APFS only; ENOTSUP/EINVAL elsewhere). On other platforms they return `CloneError::Unsupported` / `ErrorKind::Unsupported`.
- Network filesystems: the `netfs` module reports whether a path is on one (`is_network_filesystem`). Check it where lock semantics matter.

## Tests

- `tests/e2e.rs`: real files, real child processes, real symlink attacks and real SIGKILLs. Nothing is mocked.
- `tests/lock_spawn.rs`: locks across forking threads, and in-process `flock` semantics.
- `tests/lock_cross_process.rs`: locks held by other processes, for both kinds.
- `tests/lock_owner.rs`: owner record shapes (RFC 3339 and unix-millis) and `probe_owner`.
- `tests/state_durability.rs`: `StateDir` directory-sync failures, name caps, sync policy, listing kinds, options, unlimited reads, concurrent listings.
- `tests/state_ancestors.rs`: `StateDir::create` syncs the parent of each directory it creates before descending (event order), syncs nothing for directories that exist (made by another process, moved into place, or by an earlier call), and a failed sync stops the walk.
- `tests/acl_macos.rs`: macOS ACLs under the owner-only guarantee (refusal under an inheriting `allow` parent, acceptance of deny-only ACLs, `restrict_owner_only` refusing an `allow` entry and not repairing it, verification).
- `tests/atomic_publish.rs`: order of flush, replace and directory sync; owner-only before publication; Windows fail-closed hardening; typed post-publication errors.
- `tests/atomic_namespace.rs`: the temp namespace is reserved; a sweep never deletes a committed record.
- `tests/lock_alias.rs`: hard links, case aliases and symlinked directories are one lock.
- `tests/lock_reparse.rs`: a link at the lock file's name is refused.
- `tests/clone_fallback.rs`: clone, link and copy outcomes through the step seam; refusals; `fresh_metadata` gives a writable copy of a read-only source and never runs the clone or link step.
- `tests/clone.rs`: `clonefile` behaviour.
- `tests/owner_append.rs`, `tests/owner_executable.rs`, `tests/owner_report.rs`, `tests/owner_verified.rs`: the M8 additions above, each with its refusal and failure cases.
- `tests/swap.rs`: `RENAME_SWAP` exchange, missing-path error, and the `rightkit-swap` binary.
- Unit tests in `src/contain.rs` (containment and confined opens, adversarial `..`, absolute, NUL, symlink, dangling, loop, hard-link, FIFO and Windows device-name cases), `src/handle.rs`, `src/names.rs`, `src/publish_dir.rs`, `src/zip.rs` (CRC vectors and a parse of the written layout), `src/trash.rs` (record parsing; a macOS round trip through the real Trash) and the DACL policy in `src/private.rs`.

## Version and changes

Published version: see [INDEX.md](../../INDEX.md). Version history: [CHANGELOG.md](CHANGELOG.md).

## BLAKE3 identity & verified-copy progress (G8)

```toml
rightkit-fs = { version = "0.4.4", default-features = false, features = ["fs", "blake3"] }
```

```rust,ignore
use rightkit_fs::{copy_verified_with_digest, AtomicWriteOptions, DigestAlgorithm};
let (identity, bytes) = rightkit_fs::digest::blake3_file_with_progress(
    source, |read, total| report_hash_bytes(read, total),
)?;
let copied = copy_verified_with_digest(
    source, destination, AtomicWriteOptions::default(), DigestAlgorithm::Blake3,
    Some(&identity), |written, total| report_copy_bytes(written, total),
)?;
assert_eq!(copied.digest, identity);
assert_eq!(copied.bytes, bytes);
```

Callbacks run synchronously at zero & after each chunk (up to 64 KiB), with byte count
& initial file length. Empty files report `(0, 0)` once. Metadata length is a snapshot;
hash functions return bytes actually read. Verified copies reject a changed length.
Copy progress covers writes to temp: the final callback precedes verification & publication.
Treat successful return as completion. Failed verification preserves destination contents;
`AtomicWriter` can report directory-sync or cleanup errors after replacement.

Existing SHA-256 `copy_verified` & `VerifiedCopy.sha256` stay compatible.
`copy_verified_with_progress` supplies byte progress for that API. For algorithm selection,
`copy_verified_with_digest` returns `VerifiedDigestCopy { algorithm, digest, bytes }`.

## Deterministic STORED ZIP (G9)

Call `rightkit_fs::zip::write_zip(destination, &entries)` with
`entries: Vec<(String, PathBuf)>`. Order is preserved; sort entries before calling if
ordering starts from directory enumeration. Identical names, order & source bytes produce
identical output regardless of clock, source mtime, permissions or host OS. Encoding matches
CutRight’s former `cutright-qa/src/zip.rs`: STORED, UTF-8 flag, DOS epoch 1980-01-01,
Unix creator 3.0, mode 0644, no extras, comments or data descriptors.

ZIP64 is unsupported: fewer than 65535 entries, names at most 65535 UTF-8 bytes, sizes
& offsets below 0xffffffff. Oversized inputs return `io::ErrorKind::InvalidInput`.
Sources must stay unchanged during CRC & copy passes. Names are stored verbatim;
callers select safe archive names. Destination is truncated directly & must not alias
sources; failures may leave partial archives. Writes flush buffers without fsync.
`read_zip` verifies this writer’s format & CRCs, loading contents into memory.
