//! Recursive directory copy that refuses to overwrite by default and keeps symlinks as symlinks.
//!
//! [`copy_tree`] copies a file, a symlink or a directory tree to a destination that does not exist
//! yet. It is not atomic as a whole: a failure part-way leaves the entries already copied in place and
//! returns the error, so a caller that needs all-or-nothing copies into a fresh staging directory and
//! publishes that with [`crate::rename_exclusive`] or [`crate::swap_paths`].
//!
//! Each file is written to an exclusive (`O_EXCL`) sibling temporary file and then renamed onto its
//! destination, so a destination that is a symlink or a hard link is replaced as an entry and never
//! written through: its target (or the source, when the destination is a hard link to it) is not
//! truncated. A copy whose destination resolves to the source or below it is refused before anything
//! is created.

use crate::error::{FsError, Result};
use crate::file_key::FileKey;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// How [`copy_tree`] treats existing destinations and symlinks. Both fields are explicit: there is
/// no default, because either choice can lose or silently keep data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyOptions {
    /// When true, an entry that already exists at its destination stops the copy with
    /// [`FsError::AlreadyExists`]; nothing existing is ever written over. When false, an existing
    /// destination file or symlink is replaced, and an existing destination directory is merged
    /// into.
    pub no_overwrite: bool,
    /// When true, a symlink is recreated as a symlink with the same target text, whatever it points
    /// at. When false, a symlink to a file is copied as that file's bytes, and a symlink to a
    /// directory is refused (it is not followed, so a link cycle cannot make the copy run forever).
    pub keep_symlinks: bool,
}

impl CopyOptions {
    /// Options with both choices named, for a call site that reads clearly.
    pub const fn new(no_overwrite: bool, keep_symlinks: bool) -> Self {
        Self {
            no_overwrite,
            keep_symlinks,
        }
    }
}

/// Copy the file, symlink or directory tree at `src` to `dst`.
///
/// Directories are created before their contents; a file keeps its permission bits; a symlink is
/// handled as [`CopyOptions::keep_symlinks`] says. Entries are copied in directory order, not
/// sorted. `dst` must not be the source or inside it: both are resolved through every existing
/// ancestor (relative or absolute spelling, symlinked parents, case) before anything is created, and a
/// path that cannot be resolved is refused. `src` must exist. Errors carry the path they concern.
pub fn copy_tree(src: &Path, dst: &Path, options: CopyOptions) -> Result<()> {
    let inside = || {
        FsError::io(
            dst,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the destination is inside the source tree",
            ),
        )
    };
    // Lexical first (cheap, and it holds even if resolution later fails).
    if dst.starts_with(src) {
        return Err(inside());
    }
    let src_meta = fs::symlink_metadata(src).map_err(|e| FsError::io(src, e))?;
    if src_meta.is_dir() {
        let absolute = |p: &Path| std::path::absolute(p).map_err(|e| FsError::io(p, e));
        let (real_src, abs_dst) = (absolute(src)?, absolute(dst)?);
        match crate::contain::resolved_within(&real_src, &abs_dst) {
            Ok(false) => {}
            Ok(true) => return Err(inside()),
            Err(e) => return Err(FsError::io(dst, io::Error::from(e))),
        }
    }
    copy_entry(src, dst, options)
}

fn copy_entry(src: &Path, dst: &Path, options: CopyOptions) -> Result<()> {
    let meta = fs::symlink_metadata(src).map_err(|e| FsError::io(src, e))?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        if options.keep_symlinks {
            return copy_link(src, dst, options.no_overwrite);
        }
        let followed = fs::metadata(src).map_err(|e| FsError::io(src, e))?;
        if followed.is_dir() {
            return Err(FsError::io(
                src,
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "a symlink to a directory is not followed by copy_tree",
                ),
            ));
        }
        return copy_file(src, dst, options.no_overwrite, &followed);
    }
    if kind.is_dir() {
        return copy_dir(src, dst, options);
    }
    if kind.is_file() {
        return copy_file(src, dst, options.no_overwrite, &meta);
    }
    Err(FsError::NotAFile(src.to_path_buf()))
}

fn copy_dir(src: &Path, dst: &Path, options: CopyOptions) -> Result<()> {
    let created = match fs::create_dir(dst) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if options.no_overwrite {
                return Err(FsError::AlreadyExists(dst.to_path_buf()));
            }
            let existing = fs::symlink_metadata(dst).map_err(|e| FsError::io(dst, e))?;
            if !existing.is_dir() {
                return Err(FsError::io(
                    dst,
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "a non-directory exists where the directory should go",
                    ),
                ));
            }
            false
        }
        Err(e) => return Err(FsError::io(dst, e)),
    };
    let entries = fs::read_dir(src).map_err(|e| FsError::io(src, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| FsError::io(src, e))?;
        copy_entry(&entry.path(), &dst.join(entry.file_name()), options)?;
    }
    // Permissions last: a read-only source directory must not stop its own contents being written.
    // Only a directory this call created takes the source's permissions.
    if created {
        let perms = fs::metadata(src)
            .map_err(|e| FsError::io(src, e))?
            .permissions();
        fs::set_permissions(dst, perms).map_err(|e| FsError::io(dst, e))?;
    }
    Ok(())
}

fn copy_file(src: &Path, dst: &Path, no_overwrite: bool, meta: &fs::Metadata) -> Result<()> {
    let mut input = File::open(src).map_err(|e| FsError::io(src, e))?;
    // A destination that is the very file being read (a hard link to it) is refused, not replaced
    // by a lookalike. A symlink at `dst` is not an alias of its own: it is replaced as an entry.
    if let Ok(existing) = fs::symlink_metadata(dst) {
        if no_overwrite {
            return Err(FsError::AlreadyExists(dst.to_path_buf()));
        }
        if !existing.file_type().is_symlink() {
            if let (Ok(a), Ok(b)) = (FileKey::of_file(&input, src), FileKey::of_path(dst)) {
                if a == b {
                    return Err(FsError::io(
                        dst,
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "the destination is the source file",
                        ),
                    ));
                }
            }
        }
    }
    let (temp, mut output) = create_sibling_temp(dst, meta)?;
    let result = (|| -> Result<()> {
        io::copy(&mut input, &mut output).map_err(|e| FsError::io(&temp, e))?;
        output
            .set_permissions(meta.permissions())
            .map_err(|e| FsError::io(&temp, e))?;
        drop(output);
        if no_overwrite {
            crate::atomic::rename_exclusive(&temp, dst)
        } else {
            fs::rename(&temp, dst).map_err(|e| FsError::io(dst, e))
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// A fresh name next to `dst` (same directory, so a rename onto `dst` stays on one volume).
fn sibling_temp_name(dst: &Path, tag: &str) -> Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = match dst.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = dst
        .file_name()
        .ok_or_else(|| FsError::BadName(dst.display().to_string()))?
        .to_string_lossy()
        .chars()
        .take(100)
        .collect::<String>();
    Ok(dir.join(format!(
        ".{name}.rk-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )))
}

/// Create a fresh file next to `dst` (same directory, so a rename onto `dst` stays on one volume)
/// with `create_new`, which never follows a link and never reuses a name. On Unix it is created with
/// the source's permission bits so it is never wider than the file it copies.
fn create_sibling_temp(dst: &Path, meta: &fs::Metadata) -> Result<(PathBuf, File)> {
    let mut last = None;
    for _ in 0..16 {
        let temp = sibling_temp_name(dst, "copy")?;
        let mut open = OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            open.mode(meta.permissions().mode() & 0o7777);
        }
        #[cfg(not(unix))]
        let _ = meta;
        match open.open(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(FsError::io(&temp, e)),
        }
    }
    Err(FsError::io(
        dst,
        last.unwrap_or_else(|| io::Error::other("no free temporary name")),
    ))
}

fn copy_link(src: &Path, dst: &Path, no_overwrite: bool) -> Result<()> {
    let target = fs::read_link(src).map_err(|e| FsError::io(src, e))?;
    // Whether the link points at a directory decides the Windows link kind; a dangling link is a
    // file link.
    let target_is_dir = fs::metadata(src).map(|m| m.is_dir()).unwrap_or(false);
    if no_overwrite {
        // Creating a link never follows or replaces what is at `dst`.
        return create_link(&target, dst, target_is_dir).map_err(|e| map_exists(dst, e, true));
    }
    if let Ok(existing) = fs::symlink_metadata(dst) {
        if existing.is_dir() && !existing.file_type().is_symlink() {
            return Err(FsError::io(
                dst,
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a directory exists where the link should go",
                ),
            ));
        }
    }
    // Build the link under an exclusive temporary name, then rename it over `dst` (replacing the
    // entry, never what an existing link at `dst` points to).
    let mut attempts = 0;
    let temp = loop {
        let temp = sibling_temp_name(dst, "link")?;
        match create_link(&target, &temp, target_is_dir) {
            Ok(()) => break temp,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempts < 16 => attempts += 1,
            Err(e) => return Err(FsError::io(&temp, e)),
        }
    };
    if let Err(e) = fs::rename(&temp, dst) {
        let _ = fs::remove_file(&temp);
        return Err(FsError::io(dst, e));
    }
    Ok(())
}

#[cfg(unix)]
fn create_link(target: &Path, dst: &Path, _target_is_dir: bool) -> io::Result<()> {
    std::os::unix::fs::symlink(target, dst)
}

#[cfg(windows)]
fn create_link(target: &Path, dst: &Path, target_is_dir: bool) -> io::Result<()> {
    if target_is_dir {
        std::os::windows::fs::symlink_dir(target, dst)
    } else {
        std::os::windows::fs::symlink_file(target, dst)
    }
}

#[cfg(not(any(unix, windows)))]
fn create_link(_target: &Path, dst: &Path, _target_is_dir: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("symlinks are not supported for {}", dst.display()),
    ))
}

/// An `AlreadyExists` from a create at `dst` becomes [`FsError::AlreadyExists`] when the caller
/// asked for no overwrite; every other error keeps its path and kind.
fn map_exists(dst: &Path, error: io::Error, no_overwrite: bool) -> FsError {
    if no_overwrite && error.kind() == io::ErrorKind::AlreadyExists {
        FsError::AlreadyExists(dst.to_path_buf())
    } else {
        FsError::io(dst, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn copies_a_nested_tree_with_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        write(&src.join("a/b/leaf.txt"), "leaf");
        write(&src.join("top.txt"), "top");
        let dst = dir.path().join("dst");
        copy_tree(&src, &dst, CopyOptions::new(true, true)).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("a/b/leaf.txt")).unwrap(),
            "leaf"
        );
        assert_eq!(std::fs::read_to_string(dst.join("top.txt")).unwrap(), "top");
    }

    #[test]
    fn refuses_to_overwrite_an_existing_file_and_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        write(&src.join("f"), "new");
        let dst = dir.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        write(&dst.join("f"), "old");
        let err = copy_tree(&src, &dst, CopyOptions::new(true, true)).unwrap_err();
        assert!(matches!(err, FsError::AlreadyExists(_)), "{err:?}");
        assert_eq!(std::fs::read_to_string(dst.join("f")).unwrap(), "old");
    }

    #[test]
    fn overwrite_mode_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        write(&src.join("f"), "new");
        let dst = dir.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        write(&dst.join("f"), "old");
        copy_tree(&src, &dst, CopyOptions::new(false, true)).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("f")).unwrap(), "new");
    }

    #[test]
    fn refuses_a_destination_inside_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let err = copy_tree(&src, &src.join("inner"), CopyOptions::new(true, true)).unwrap_err();
        assert_eq!(err.kind(), Some(io::ErrorKind::InvalidInput));
    }

    #[cfg(unix)]
    #[test]
    fn keeps_a_symlink_as_a_symlink_and_refuses_a_directory_link_when_not_keeping() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        write(&src.join("real"), "bytes");
        std::os::unix::fs::symlink("real", src.join("link")).unwrap();
        let kept = dir.path().join("kept");
        copy_tree(&src, &kept, CopyOptions::new(true, true)).unwrap();
        assert!(std::fs::symlink_metadata(kept.join("link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(kept.join("link")).unwrap(),
            Path::new("real")
        );

        std::fs::create_dir(src.join("sub")).unwrap();
        std::os::unix::fs::symlink("sub", src.join("dirlink")).unwrap();
        let followed = dir.path().join("followed");
        let err = copy_tree(&src, &followed, CopyOptions::new(true, false)).unwrap_err();
        assert_eq!(err.kind(), Some(io::ErrorKind::Unsupported));
    }

    #[test]
    fn a_failed_no_overwrite_copy_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        write(&src.join("f"), "new");
        let dst = dir.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        write(&dst.join("f"), "old");
        copy_tree(&src, &dst, CopyOptions::new(true, true)).unwrap_err();
        let names: Vec<_> = std::fs::read_dir(&dst)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("f")]);
    }

    #[test]
    fn a_destination_hard_linked_to_the_source_is_refused_and_the_source_survives() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        write(&src, "precious");
        let dst = dir.path().join("alias.txt");
        std::fs::hard_link(&src, &dst).unwrap();
        let err = copy_tree(&src, &dst, CopyOptions::new(false, true)).unwrap_err();
        assert_eq!(err.kind(), Some(io::ErrorKind::InvalidInput), "{err:?}");
        assert_eq!(std::fs::read_to_string(&src).unwrap(), "precious");
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "precious");
    }

    #[test]
    fn a_dot_dot_spelling_of_a_destination_inside_the_source_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        write(&src.join("f"), "x");
        std::fs::create_dir(dir.path().join("other")).unwrap();
        // Not lexically below `src`, but resolves to `src/inner`.
        let sneaky = dir.path().join("other/../src/inner");
        assert!(!sneaky.starts_with(&src));
        let err = copy_tree(&src, &sneaky, CopyOptions::new(true, true)).unwrap_err();
        assert_eq!(err.kind(), Some(io::ErrorKind::InvalidInput), "{err:?}");
        assert!(!src.join("inner").exists(), "nothing was created");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_leading_into_the_source_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        write(&src.join("f"), "x");
        std::os::unix::fs::symlink(&src, dir.path().join("door")).unwrap();
        let through = dir.path().join("door").join("inner");
        let err = copy_tree(&src, &through, CopyOptions::new(true, true)).unwrap_err();
        assert_eq!(err.kind(), Some(io::ErrorKind::InvalidInput), "{err:?}");
        assert!(!src.join("inner").exists(), "nothing was created");
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_whose_parent_cannot_be_resolved_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("dangling"))
            .unwrap();
        let err = copy_tree(
            &src,
            &dir.path().join("dangling").join("x"),
            CopyOptions::new(true, true),
        )
        .unwrap_err();
        assert!(err.kind().is_some(), "{err:?}");
        assert!(!dir.path().join("missing").exists());
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_replaces_a_destination_symlink_and_never_writes_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        write(&outside, "outside must stay");
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        write(&src.join("f"), "fresh");
        let dst = dir.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        std::os::unix::fs::symlink(&outside, dst.join("f")).unwrap();
        copy_tree(&src, &dst, CopyOptions::new(false, true)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "outside must stay"
        );
        let meta = std::fs::symlink_metadata(dst.join("f")).unwrap();
        assert!(meta.is_file(), "the link entry was replaced by a file");
        assert_eq!(std::fs::read_to_string(dst.join("f")).unwrap(), "fresh");
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_with_a_kept_symlink_replaces_the_destination_link_not_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        write(&outside, "outside must stay");
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::os::unix::fs::symlink("elsewhere", src.join("l")).unwrap();
        let dst = dir.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        std::os::unix::fs::symlink(&outside, dst.join("l")).unwrap();
        copy_tree(&src, &dst, CopyOptions::new(false, true)).unwrap();
        assert_eq!(
            std::fs::read_link(dst.join("l")).unwrap(),
            Path::new("elsewhere")
        );
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "outside must stay"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_copied_file_is_never_wider_than_its_source_while_it_is_written() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("secret");
        write(&src, "s");
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o600)).unwrap();
        let dst = dir.path().join("copy");
        copy_tree(&src, &dst, CopyOptions::new(true, true)).unwrap();
        let mode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
