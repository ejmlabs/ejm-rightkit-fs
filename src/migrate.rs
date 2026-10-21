//! Move a whole directory to a new place without ever showing a half-moved destination.
//!
//! [`migrate_dir`] (also `AppDirs::migrate_dir` with feature `app-dirs`) is for the folder an app
//! used to keep its data in and now keeps somewhere else (a renamed app folder, a data folder that
//! moves from Local to Roaming, a legacy name). The destination is never visible in a partial state:
//!
//! | Situation | Mechanism | What a crash or a reader can see |
//! |---|---|---|
//! | `from` and `to` on one volume | one `rename` of the directory | the whole tree at `from` or the whole tree at `to` |
//! | different volumes | the tree is copied to a hidden sibling of `to` (`.<name>.rkmigrate-<pid>-<n>`), the sibling is renamed to `to`, then `from` is removed | `to` appears whole or not at all; a crash before the rename leaves `from` untouched and a stray hidden copy (swept by the next call once its process is gone); a crash after it leaves both complete trees |
//!
//! Rules:
//!
//! * `from` must be a real directory (a symlink or a file is refused, never followed).
//! * `to` must be absent or an **empty** real directory. A directory that holds anything is
//!   refused with [`FsError::AlreadyExists`] and nothing is written; nothing is ever merged or
//!   overwritten. `to` may not be inside `from` or the same path; this is decided on canonical
//!   paths (symlinks in `from` or in the existing part of `to` are resolved, case is ignored on
//!   Windows) before anything is created.
//! * Missing parents of `to` are created.
//! * Interrupted, the call is safe to repeat: with `from` untouched it migrates again; with the move
//!   done and `from` still present (`source_removed` was false, or the crash came after the rename)
//!   `to` is populated, so a repeat is refused and the caller removes the leftover `from` itself.
//!
//! Limits: a directory that another process holds open (a Windows working directory inside it, an
//! indexer) can refuse the rename or the final removal; the error is returned. Symlinks inside the
//! tree are kept as symlinks on the copy path. On the copy path every copied file is flushed
//! (`sync_all`) and, on Unix, every directory of the copy is synced before the publishing rename;
//! `from` is removed only after that rename succeeded and the parent directory of `to` was synced
//! (if that sync fails, `from` is kept and `source_removed` is false). Windows cannot sync a
//! directory, so only files are flushed there. Not compiled or run when written.

use crate::error::{FsError, Result};
use crate::tree::{copy_tree, CopyOptions};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// How [`migrate_dir`] moved the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MigrateMethod {
    /// One `rename` of the directory (same volume): atomic.
    Rename,
    /// `from` and `to` are on different volumes: the tree was copied to a hidden sibling of `to`,
    /// the sibling renamed to `to`, then `from` removed.
    CopyThenRemove,
}

/// What [`migrate_dir`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Migrated {
    /// The mechanism that ran.
    pub method: MigrateMethod,
    /// False only on the copy path when `to` is complete but the old tree could not be removed
    /// entirely (a file in use); the caller may retry `std::fs::remove_dir_all(from)`.
    pub source_removed: bool,
}

/// Move the directory `from` to `to`. See the module docs for the rules and the guarantee.
pub fn migrate_dir(from: &Path, to: &Path) -> Result<Migrated> {
    let meta = fs::symlink_metadata(from).map_err(|e| FsError::io(from, e))?;
    if meta.file_type().is_symlink() {
        return Err(FsError::Symlink(from.to_path_buf()));
    }
    if !meta.is_dir() {
        return Err(FsError::NotADirectory(from.to_path_buf()));
    }
    let abs_from = std::path::absolute(from).map_err(|e| FsError::io(from, e))?;
    let abs_to = std::path::absolute(to).map_err(|e| FsError::io(to, e))?;
    refuse_inside(&abs_from, &abs_to, to)?;
    let parent = match abs_to.parent() {
        Some(p) => p.to_path_buf(),
        None => return Err(FsError::BadName(to.display().to_string())),
    };
    let name = abs_to
        .file_name()
        .ok_or_else(|| FsError::BadName(to.display().to_string()))?
        .to_string_lossy()
        .chars()
        .take(100)
        .collect::<String>();
    // Only now, with the destination known to be outside the source, are parents created.
    fs::create_dir_all(&parent).map_err(|e| FsError::io(&parent, e))?;
    let existing = check_destination(&abs_to)?;

    // Same volume: one rename. A directory rename never replaces a non-empty directory (Unix, and
    // the POSIX-semantics rename current std uses on Windows), so a destination that was filled
    // meanwhile fails the call with nothing lost.
    match rename_into_place(&abs_from, &abs_to, existing) {
        Ok(()) => {
            let _ = crate::atomic::try_sync_dir(&parent);
            return Ok(Migrated {
                method: MigrateMethod::Rename,
                source_removed: true,
            });
        }
        Err(e) if is_cross_device(&e) => {}
        Err(e) if is_occupied(&e) => return Err(FsError::AlreadyExists(abs_to)),
        Err(e) => return Err(FsError::io(&abs_to, e)),
    }

    // Different volumes: copy to a hidden sibling, make it durable, publish it with one rename,
    // then remove `from`.
    sweep_stale_copies(&parent, &name);
    let temp = parent.join(format!(
        ".{name}.rkmigrate-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(e) = copy_tree(&abs_from, &temp, CopyOptions::new(true, true)) {
        let _ = fs::remove_dir_all(&temp);
        return Err(e);
    }
    if let Err(e) = sync_tree(&temp) {
        let _ = fs::remove_dir_all(&temp);
        return Err(e);
    }
    // The copy may have taken long: look at the destination again right before publishing.
    let published = check_destination(&abs_to).and_then(|existing| {
        rename_into_place(&temp, &abs_to, existing).map_err(|e| classify(&abs_to, e))
    });
    if let Err(e) = published {
        let _ = fs::remove_dir_all(&temp);
        return Err(e);
    }
    // `from` is the only other copy: remove it only once `to` is durably named.
    let source_removed =
        crate::atomic::try_sync_dir(&parent).is_ok() && fs::remove_dir_all(&abs_from).is_ok();
    Ok(Migrated {
        method: MigrateMethod::CopyThenRemove,
        source_removed,
    })
}

/// Refuse a destination that is the source or inside it. Decided on canonical paths: `from` is
/// canonicalized, and `to` through its deepest existing ancestor (the missing tail is appended
/// as is, and may not hold `..`), so a symlink into the source is seen. Nothing is created.
fn refuse_inside(abs_from: &Path, abs_to: &Path, to: &Path) -> Result<()> {
    let canon_from =
        crate::plain::canonicalize_plain(abs_from).map_err(|e| FsError::io(abs_from, e))?;
    let canon_to = canonical_with_missing_tail(abs_to).map_err(|e| FsError::io(to, e))?;
    if is_inside(&canon_to, &canon_from) {
        return Err(FsError::io(
            to,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the destination is the source or inside it",
            ),
        ));
    }
    Ok(())
}

fn canonical_with_missing_tail(path: &Path) -> io::Result<PathBuf> {
    for ancestor in path.ancestors() {
        if fs::symlink_metadata(ancestor).is_ok() {
            let mut out = crate::plain::canonicalize_plain(ancestor)?;
            let tail = path.strip_prefix(ancestor).unwrap_or(Path::new(""));
            for component in tail.components() {
                match component {
                    Component::Normal(part) => out.push(part),
                    Component::CurDir => {}
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "the missing part of the destination path holds `..`",
                        ))
                    }
                }
            }
            return Ok(out);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no part of the destination path exists",
    ))
}

/// True when `child` is `root` or below it, whole components, case-insensitive on Windows.
fn is_inside(child: &Path, root: &Path) -> bool {
    fn fold(component: Component<'_>) -> String {
        let text = component.as_os_str().to_string_lossy().into_owned();
        if cfg!(windows) {
            text.to_lowercase()
        } else {
            text
        }
    }
    let mut child = child.components().map(fold);
    root.components().map(fold).all(|r| child.next() == Some(r))
}

/// Flush the copy: every regular file, and on Unix every directory (Windows cannot sync one).
fn sync_tree(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(|e| FsError::io(dir, e))? {
        let entry = entry.map_err(|e| FsError::io(dir, e))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| FsError::io(&path, e))?;
        if kind.is_dir() {
            sync_tree(&path)?;
        } else if kind.is_file() {
            sync_file(&path).map_err(|e| FsError::io(&path, e))?;
        }
    }
    sync_directory(dir).map_err(|e| FsError::io(dir, e))
}

fn sync_file(path: &Path) -> io::Result<()> {
    // Windows needs write access for `FlushFileBuffers`; nothing is written.
    match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file.sync_all(),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            // A read-only file: flush through a read handle where the platform allows it.
            match fs::File::open(path)?.sync_all() {
                Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied => Ok(()),
                other => other,
            }
        }
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn sync_directory(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> io::Result<()> {
    // Directory handles cannot be flushed on Windows through std; the files were flushed above.
    Ok(())
}

/// `to` must be absent or an empty real directory. Returns whether an empty directory is there.
/// Nothing is removed here: on Unix a rename onto an empty directory works, and on Windows
/// [`rename_into_place`] removes it only right before the rename.
fn check_destination(to: &Path) -> Result<bool> {
    match fs::symlink_metadata(to) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(FsError::io(to, e)),
        Ok(meta) if meta.file_type().is_symlink() => Err(FsError::Symlink(to.to_path_buf())),
        Ok(meta) if !meta.is_dir() => Err(FsError::AlreadyExists(to.to_path_buf())),
        Ok(_) => {
            let mut entries = fs::read_dir(to).map_err(|e| FsError::io(to, e))?;
            if entries.next().is_some() {
                return Err(FsError::AlreadyExists(to.to_path_buf()));
            }
            Ok(true)
        }
    }
}

/// Rename `src` to `to`. `existing_empty` says an empty directory occupies `to`.
#[cfg(not(windows))]
fn rename_into_place(src: &Path, to: &Path, _existing_empty: bool) -> io::Result<()> {
    fs::rename(src, to)
}

/// Windows: the empty directory is removed right before the rename and recreated if it fails, so
/// the caller's empty `to` is never lost.
#[cfg(windows)]
fn rename_into_place(src: &Path, to: &Path, existing_empty: bool) -> io::Result<()> {
    if existing_empty {
        // `remove_dir` fails on a directory that became non-empty, so nothing is lost.
        fs::remove_dir(to)?;
        let result = fs::rename(src, to);
        if result.is_err() {
            let _ = fs::create_dir(to);
        }
        result
    } else {
        fs::rename(src, to)
    }
}

fn classify(path: &Path, e: io::Error) -> FsError {
    if is_occupied(&e) {
        FsError::AlreadyExists(path.to_path_buf())
    } else {
        FsError::io(path, e)
    }
}

/// The destination exists and holds something (`ENOTEMPTY`, `EEXIST`, or the Windows equivalents).
fn is_occupied(e: &io::Error) -> bool {
    if matches!(
        e.kind(),
        io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
    ) {
        return true;
    }
    #[cfg(windows)]
    {
        // ERROR_ALREADY_EXISTS (183), ERROR_DIR_NOT_EMPTY (145), ERROR_FILE_EXISTS (80)
        matches!(e.raw_os_error(), Some(183 | 145 | 80))
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The rename crossed a volume boundary (`EXDEV`, `ERROR_NOT_SAME_DEVICE`).
fn is_cross_device(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::CrossesDevices {
        return true;
    }
    #[cfg(windows)]
    {
        e.raw_os_error() == Some(17)
    }
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EXDEV)
    }
    #[cfg(not(any(windows, unix)))]
    {
        false
    }
}

/// Remove hidden copies an earlier call left behind, but only those whose process is confirmed gone.
fn sweep_stale_copies(parent: &Path, name: &str) {
    let prefix = format!(".{name}.rkmigrate-");
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some(rest) = file_name.strip_prefix(prefix.as_str()) else {
            continue;
        };
        let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if pid != std::process::id()
            && crate::process_liveness(pid) == crate::ProcessLiveness::Dead
            && entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(dir: &Path) {
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), b"alpha").unwrap();
        fs::write(dir.join("sub").join("b.txt"), b"beta").unwrap();
    }

    fn assert_filled(dir: &Path) {
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(dir.join("sub").join("b.txt")).unwrap(), b"beta");
    }

    #[test]
    fn same_volume_move_is_one_rename() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("old");
        let to = root.path().join("nested").join("new");
        fill(&from);
        let done = migrate_dir(&from, &to).unwrap();
        assert_eq!(done.method, MigrateMethod::Rename);
        assert!(done.source_removed);
        assert!(!from.exists());
        assert_filled(&to);
    }

    #[test]
    fn empty_destination_directory_is_replaced() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("old");
        let to = root.path().join("new");
        fill(&from);
        fs::create_dir(&to).unwrap();
        migrate_dir(&from, &to).unwrap();
        assert!(!from.exists());
        assert_filled(&to);
    }

    #[test]
    fn non_empty_destination_is_refused_and_untouched() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("old");
        let to = root.path().join("new");
        fill(&from);
        fs::create_dir(&to).unwrap();
        fs::write(to.join("keep.txt"), b"mine").unwrap();
        let err = migrate_dir(&from, &to).unwrap_err();
        assert!(matches!(err, FsError::AlreadyExists(_)), "{err:?}");
        assert_filled(&from);
        assert_eq!(fs::read(to.join("keep.txt")).unwrap(), b"mine");
        assert!(!to.join("a.txt").exists());
    }

    #[test]
    fn file_destination_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("old");
        let to = root.path().join("new");
        fill(&from);
        fs::write(&to, b"file").unwrap();
        assert!(matches!(
            migrate_dir(&from, &to).unwrap_err(),
            FsError::AlreadyExists(_)
        ));
        assert_filled(&from);
    }

    #[test]
    fn missing_source_and_file_source_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let to = root.path().join("new");
        assert!(migrate_dir(&root.path().join("nope"), &to)
            .unwrap_err()
            .is_not_found());
        let file = root.path().join("f");
        fs::write(&file, b"x").unwrap();
        assert!(matches!(
            migrate_dir(&file, &to).unwrap_err(),
            FsError::NotADirectory(_)
        ));
        assert!(!to.exists());
    }

    #[test]
    fn destination_inside_or_equal_to_source_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("old");
        fill(&from);
        assert!(migrate_dir(&from, &from).is_err());
        assert!(migrate_dir(&from, &from.join("sub").join("x")).is_err());
        assert_filled(&from);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_source_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        fill(&real);
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            migrate_dir(&link, &root.path().join("new")).unwrap_err(),
            FsError::Symlink(_)
        ));
        assert_filled(&real);
    }

    #[test]
    fn error_classification() {
        #[cfg(unix)]
        assert!(is_cross_device(&io::Error::from_raw_os_error(libc::EXDEV)));
        #[cfg(windows)]
        assert!(is_cross_device(&io::Error::from_raw_os_error(17)));
        assert!(!is_cross_device(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(is_occupied(&io::Error::from(io::ErrorKind::AlreadyExists)));
        assert!(is_occupied(&io::Error::from(
            io::ErrorKind::DirectoryNotEmpty
        )));
    }

    #[test]
    fn stale_hidden_copy_of_a_dead_process_is_swept_and_live_one_kept() {
        let root = tempfile::tempdir().unwrap();
        let dead = dead_pid();
        let stale = root.path().join(format!(".new.rkmigrate-{dead}-0"));
        let ours = root
            .path()
            .join(format!(".new.rkmigrate-{}-999999", std::process::id()));
        let other = root.path().join(".other.rkmigrate-1-0");
        for d in [&stale, &ours, &other] {
            fs::create_dir_all(d.join("x")).unwrap();
        }
        sweep_stale_copies(root.path(), "new");
        assert!(!stale.exists());
        assert!(ours.exists());
        assert!(other.exists());
    }

    /// The copy path as the cross-volume branch runs it: copy to a hidden sibling, publish by
    /// rename, remove the source. The destination is absent until the single rename, and a copy
    /// that fails leaves the source whole.
    #[test]
    fn copy_publish_remove_sequence_and_failed_copy_keeps_source() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("old");
        let temp = root.path().join(".new.rkmigrate-1-0");
        let to = root.path().join("new");
        fill(&from);
        copy_tree(&from, &temp, CopyOptions::new(true, true)).unwrap();
        assert!(!to.exists());
        fs::rename(&temp, &to).unwrap();
        fs::remove_dir_all(&from).unwrap();
        assert_filled(&to);

        let from2 = root.path().join("old2");
        fill(&from2);
        let taken = root.path().join(".taken");
        fs::create_dir(&taken).unwrap();
        assert!(copy_tree(&from2, &taken, CopyOptions::new(true, true)).is_err());
        assert_filled(&from2);
    }

    #[test]
    fn inside_check_uses_canonical_paths_and_creates_nothing() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("old");
        fill(&from);
        let deep = from.join("sub").join("deep").join("x");
        assert!(migrate_dir(&from, &deep).is_err());
        assert!(
            !from.join("sub").join("deep").exists(),
            "no parent was created"
        );
        // `..` through the source lands back inside it.
        let dotdot = root
            .path()
            .join("other")
            .join("..")
            .join("old")
            .join("sub")
            .join("n");
        assert!(refuse_inside(&from, &dotdot, &dotdot).is_err());
        // A sibling whose name merely starts with the source name is fine.
        let sibling = root.path().join("old2").join("x");
        assert!(refuse_inside(&from, &sibling, &sibling).is_ok());
        #[cfg(unix)]
        {
            let alias = root.path().join("alias");
            std::os::unix::fs::symlink(&from, &alias).unwrap();
            let via_link = alias.join("sub").join("deeper").join("x");
            assert!(migrate_dir(&from, &via_link).is_err());
            assert!(!from.join("sub").join("deeper").exists());
            assert_filled(&from);
        }
        #[cfg(windows)]
        {
            let upper = root.path().join("OLD").join("sub").join("n");
            assert!(refuse_inside(&from, &upper, &upper).is_err());
        }
    }

    #[test]
    fn is_inside_compares_whole_components() {
        assert!(is_inside(Path::new("/a/b/c"), Path::new("/a/b")));
        assert!(is_inside(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!is_inside(Path::new("/a/bc"), Path::new("/a/b")));
        assert!(!is_inside(Path::new("/a"), Path::new("/a/b")));
    }

    #[test]
    fn sync_tree_flushes_nested_files_and_reports_a_missing_tree() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("old");
        fill(&from);
        fs::write(from.join("sub").join("empty"), b"").unwrap();
        sync_tree(&from).unwrap();
        assert_filled(&from);
        assert!(sync_tree(&root.path().join("missing")).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("nowhere", from.join("dangling")).unwrap();
            sync_tree(&from).unwrap();
        }
    }

    #[test]
    fn an_empty_destination_is_kept_until_the_rename() {
        let root = tempfile::tempdir().unwrap();
        let to = root.path().join("new");
        fs::create_dir(&to).unwrap();
        assert!(check_destination(&to).unwrap());
        assert!(to.is_dir(), "checking removes nothing");
        assert!(!check_destination(&root.path().join("absent")).unwrap());
        // A failing rename leaves the empty directory in place on every platform.
        let missing_src = root.path().join("no-such-source");
        assert!(rename_into_place(&missing_src, &to, true).is_err());
        assert!(to.is_dir());
        // A successful one replaces it.
        let src = root.path().join("src");
        fill(&src);
        rename_into_place(&src, &to, true).unwrap();
        assert_filled(&to);
    }

    fn dead_pid() -> u32 {
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }
}
