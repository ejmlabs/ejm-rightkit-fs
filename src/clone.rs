//! Copy-on-write file clones, with an explicit copy ladder for callers that can accept less.
//!
//! [`clone_file_nofollow`] never falls back to a byte copy. On macOS it calls `clonefile(2)` with
//! `CLONE_NOFOLLOW`; the kernel either creates a clone on the same APFS volume or fails with a
//! typed error, so a caller that asked for a clone can never silently receive a copy. Other
//! platforms always return [`CloneError::Unsupported`].
//!
//! [`clone_or_link_or_copy_nofollow`] is the ladder: a copy-on-write clone where the platform has
//! one (macOS `clonefile`, Linux `FICLONE`), then a hard link, then a plain copy through the
//! crate's durable atomic-write path, and it reports which step produced the destination
//! ([`CloneOutcome`]). It never follows a symlink at either end and never overwrites.
//!
//! **Owner-only sources are not cloned natively on macOS.** `clonefile` creates the destination
//! itself, with the source's mode and with whatever ACL the destination's directory hands out, so
//! an owner-only destination cannot be established: a `0600` secret cloned into a directory with an
//! inheritable `allow` ACL entry would be readable the moment it exists. The macOS clone step therefore
//! declines a source whose mode has no group or other permission bit set, and the ladder goes on
//! to the hard link, then to the copy, which creates its destination `0600` first (and refuses a
//! parent whose ACL would change that). A source readable by others is cloned as before. The Linux `FICLONE` step already clones into a `0600` temp. The standalone
//! [`clone_file_nofollow`] is unchanged and is not an owner-only creation.

use crate::atomic::{atomic_write_with_options, AtomicWriteOptions, Privacy, ReplaceMode};
use crate::error::{FsError, Result as FsResult};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Why a clone was not created. Each variant maps to a stable code for callers and the helper binary.
#[derive(Debug, thiserror::Error)]
pub enum CloneError {
    /// The source is a symbolic link. It is refused, never followed or cloned as a link.
    #[error("{0} is a symbolic link; clone refuses links")]
    Symlink(PathBuf),
    /// The source is not a regular file (directory, device, socket, ...).
    #[error("{0} is not a regular file")]
    NotAFile(PathBuf),
    /// Source and destination are on different volumes; clonefile needs one volume.
    #[error("{src} and {dst} are on different volumes")]
    CrossVolume { src: PathBuf, dst: PathBuf },
    /// The volume or filesystem cannot clone (not APFS, or the kernel reports ENOTSUP/ENOSYS).
    #[error("clonefile is not supported for {path}: {source}")]
    Unsupported {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The source or destination path cannot be passed to the kernel (embedded NUL).
    #[error("path contains a NUL byte: {0}")]
    InvalidPath(PathBuf),
    /// Any other failure (missing source, destination exists, permissions, ...).
    #[error("clone of {path} failed: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl CloneError {
    /// Stable machine code, shared with the `rightkit-clonefile` helper's JSON output.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Symlink(_) => "CLONE_SYMLINK_REFUSED",
            Self::NotAFile(_) => "CLONE_NOT_A_FILE",
            Self::CrossVolume { .. } => "CLONE_CROSS_VOLUME",
            Self::Unsupported { .. } => "CLONE_UNSUPPORTED",
            Self::InvalidPath(_) => "CLONE_INVALID_PATH",
            Self::Io { .. } => "CLONE_IO",
        }
    }

    /// Exit status used by the `rightkit-clonefile` helper for this failure.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Unsupported { .. } => 3,
            Self::CrossVolume { .. } => 4,
            Self::Symlink(_) => 5,
            Self::NotAFile(_) => 6,
            Self::InvalidPath(_) => 2,
            Self::Io { .. } => 1,
        }
    }

    #[cfg(target_os = "macos")]
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Clones regular file `src` to a new path `dst` without following a symlink at `src`.
///
/// `dst` must not exist. The clone is created atomically by the kernel; on any failure no
/// partial destination is left behind and no bytes are copied.
///
/// This is **not an owner-only creation**: the clone has the source's mode and receives the ACL
/// entries `dst`'s directory hands out. Do not use it for a private source into a directory you
/// do not control; [`clone_or_link_or_copy_nofollow`] declines to clone such a source.
#[cfg(target_os = "macos")]
pub fn clone_file_nofollow(src: &Path, dst: &Path) -> Result<(), CloneError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    /// `CLONE_NOFOLLOW` from `<sys/clonefile.h>`: do not follow a symlink at the final `src` component.
    const CLONE_NOFOLLOW: u32 = 0x0001;

    if dst
        .file_name()
        .is_some_and(|n| crate::atomic::reject_reserved_name(&n.to_string_lossy()).is_err())
    {
        // A name a temp-file sweep would delete as an orphan.
        return Err(CloneError::InvalidPath(dst.to_path_buf()));
    }
    let meta = std::fs::symlink_metadata(src).map_err(|source| CloneError::io(src, source))?;
    if meta.file_type().is_symlink() {
        return Err(CloneError::Symlink(src.to_path_buf()));
    }
    if !meta.is_file() {
        return Err(CloneError::NotAFile(src.to_path_buf()));
    }
    let c_src = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| CloneError::InvalidPath(src.to_path_buf()))?;
    let c_dst = CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| CloneError::InvalidPath(dst.to_path_buf()))?;
    // SAFETY: both pointers are valid NUL-terminated strings that outlive the call; flags is a documented value.
    let rc = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), CLONE_NOFOLLOW) };
    if rc == 0 {
        return Ok(());
    }
    let source = io::Error::last_os_error();
    Err(match source.raw_os_error() {
        Some(libc::EXDEV) => CloneError::CrossVolume {
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
        },
        Some(code) if code == libc::ENOTSUP || code == libc::ENOSYS => CloneError::Unsupported {
            path: dst.to_path_buf(),
            source,
        },
        _ => CloneError::io(dst, source),
    })
}

/// Non-macOS platforms have no `clonefile(2)`; callers must treat this as unsupported and fail closed.
#[cfg(not(target_os = "macos"))]
pub fn clone_file_nofollow(src: &Path, dst: &Path) -> Result<(), CloneError> {
    Err(CloneError::Unsupported {
        path: dst.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::Unsupported,
            format!("clonefile is macOS-only (source {})", src.display()),
        ),
    })
}

/// Which step of [`clone_or_link_or_copy_nofollow`] produced the destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CloneOutcome {
    /// A copy-on-write clone: its own inode, sharing storage blocks with the source until either
    /// side is written. An in-place write to one never reaches the other.
    Cloned,
    /// A hard link: the destination is a second name for the **same inode** as the source. The two
    /// share content, so an in-place write through either name changes both, and the data lives
    /// until the last name is deleted. A caller that tracks the copies should treat linked
    /// copies as one item on delete (removing the destination removes only a name). Callers that
    /// will modify the copy in place should disallow this step ([`CloneOptions::allow_hard_link`]).
    HardLinked,
    /// A byte copy made through the crate's atomic-write path: its own inode and its own storage.
    /// Permission bits are copied on Unix (unless [`CloneOptions::fresh_metadata`] is set, which
    /// gives the platform's default mode for a new file); ownership, extended attributes and
    /// timestamps are not.
    Copied,
}

/// How [`clone_or_link_or_copy_nofollow_with`] behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloneOptions {
    /// Allow the hard-link step. Default `true`. A hard link shares content with the source (see
    /// [`CloneOutcome::HardLinked`]), so set this to `false` when the copy will be modified in
    /// place: a failed clone then goes straight to a byte copy.
    pub allow_hard_link: bool,
    /// Make the result durable: the copy step flushes the file and the directory (the crate's
    /// durable atomic-write path), and a clone or link syncs the destination's directory on Unix.
    /// Default `true`. See [`crate::publication_durability`] for what Windows can promise.
    pub durable: bool,
    /// The destination is a fresh file, not a metadata copy of the source. Default `false`.
    ///
    /// When `true`, the clone and hard-link steps are skipped (a clone copies the source's mode
    /// and extended attributes; a hard link is the source's inode and shares its metadata), so the
    /// result is always [`CloneOutcome::Copied`]. The byte copy gets the platform's default mode
    /// for a new file (`0666 & !umask` on Unix, honouring the umask), and no extended attributes
    /// or source permission bits are carried over. Use it when a read-only source must not yield
    /// a read-only export.
    pub fresh_metadata: bool,
}

impl Default for CloneOptions {
    fn default() -> Self {
        Self {
            allow_hard_link: true,
            durable: true,
            fresh_metadata: false,
        }
    }
}

impl CloneOptions {
    /// These options with the hard-link step disallowed.
    pub fn without_hard_link(self) -> Self {
        Self {
            allow_hard_link: false,
            ..self
        }
    }

    /// These options with [`CloneOptions::fresh_metadata`] set to `fresh`.
    pub fn fresh_metadata(self, fresh: bool) -> Self {
        Self {
            fresh_metadata: fresh,
            ..self
        }
    }
}

/// Creates `dst` from `src` by the cheapest way available: a copy-on-write clone, else a hard link
/// when both are on one volume, else a byte copy. [`CloneOptions::default`] applies; see
/// [`clone_or_link_or_copy_nofollow_with`].
pub fn clone_or_link_or_copy_nofollow(src: &Path, dst: &Path) -> FsResult<CloneOutcome> {
    clone_or_link_or_copy_nofollow_with(src, dst, &CloneOptions::default())
}

/// [`clone_or_link_or_copy_nofollow`] with explicit [`CloneOptions`].
///
/// - `src` must be a regular file. A symbolic link (or, on Windows, any reparse point) at `src` is
///   [`FsError::Symlink`], a non-file is [`FsError::NotAFile`]; it is never followed.
/// - `dst` must not exist. Anything at its name is refused without being touched: a symlink
///   (even a dangling one) is [`FsError::Symlink`], anything else [`FsError::AlreadyExists`]. Each
///   step also creates exclusively, so a destination that appears first still loses nothing.
/// - The first step that works wins, in this order, and the outcome says which: a clone
///   ([`CloneOutcome::Cloned`]), a hard link ([`CloneOutcome::HardLinked`], unless disallowed), a
///   copy ([`CloneOutcome::Copied`]). A step that is unavailable (another volume, a filesystem
///   without clones or links, Windows for clones, and on macOS an owner-only source, see the
///   module docs) falls through to the next; a destination that
///   already exists or an error after the destination was published does not.
/// - `dst`'s file name must not be shaped like a temp name this crate generates
///   ([`FsError::BadName`]): a sweep would delete it.
/// - The copy is made through [`crate::atomic_write_with_options`] with
///   [`ReplaceMode::NoReplace`]: a temp file, flushed, then published exclusively. Nothing is
///   visible at `dst` until the content is complete, and a failure leaves no partial `dst`.
pub fn clone_or_link_or_copy_nofollow_with(
    src: &Path,
    dst: &Path,
    options: &CloneOptions,
) -> FsResult<CloneOutcome> {
    if let Some(name) = dst.file_name() {
        // Every step below publishes `dst`; a name shaped like a generated temp name would be
        // reclaimed by a sweep.
        crate::atomic::reject_reserved_name(&name.to_string_lossy())?;
    }
    check_source(src)?;
    check_destination(dst)?;

    if options.fresh_metadata {
        // A clone copies the source's mode and extended attributes, and a hard link is the
        // source's inode; neither step can give a fresh file, so only the byte copy runs.
        copy_step(src, dst, options)?;
        return Ok(CloneOutcome::Copied);
    }

    match run_step(Step::Clone, src, dst, options) {
        StepResult::Done => return finish(dst, options, CloneOutcome::Cloned),
        StepResult::Exists => return Err(FsError::AlreadyExists(dst.to_path_buf())),
        StepResult::Failed(error) => return Err(error),
        StepResult::Unavailable => {}
    }
    if options.allow_hard_link {
        match run_step(Step::Link, src, dst, options) {
            StepResult::Done => return finish(dst, options, CloneOutcome::HardLinked),
            StepResult::Exists => return Err(FsError::AlreadyExists(dst.to_path_buf())),
            StepResult::Failed(error) => return Err(error),
            StepResult::Unavailable => {}
        }
    }
    copy_step(src, dst, options)?;
    Ok(CloneOutcome::Copied)
}

/// The two steps that can be unavailable. The copy is the floor and has no fallback.
#[derive(Clone, Copy)]
enum Step {
    Clone,
    Link,
}

enum StepResult {
    /// `dst` now exists, made by this step.
    Done,
    /// `dst` appeared first; nothing was overwritten.
    Exists,
    /// This step cannot be used here; try the next.
    Unavailable,
    /// The step failed after `dst` may have been published; do not try another. Only the Linux
    /// clone step can produce it.
    #[allow(dead_code)]
    Failed(FsError),
}

fn run_step(step: Step, src: &Path, dst: &Path, options: &CloneOptions) -> StepResult {
    #[cfg(feature = "testing")]
    if let Some(result) = testing::overridden(step, src, dst) {
        return match result {
            Ok(()) => StepResult::Done,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => StepResult::Exists,
            Err(_) => StepResult::Unavailable,
        };
    }
    match step {
        Step::Clone => clone_step(src, dst, options),
        Step::Link => match std::fs::hard_link(src, dst) {
            Ok(()) => StepResult::Done,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => StepResult::Exists,
            // EXDEV (another volume), EPERM / ENOTSUP (no links on this filesystem or by policy),
            // EMLINK: a copy can still succeed.
            Err(_) => StepResult::Unavailable,
        },
    }
}

/// Whether a native clone of `src` must not be made: the source has no group or other permission
/// bit, so its clone would be a private file whose creation this crate cannot make owner-only
/// (`clonefile` takes the source's mode and the destination directory's inheritable ACL). A source
/// whose mode cannot be read counts as private: when the answer is unknown, the copy path decides.
#[cfg(target_os = "macos")]
fn source_must_not_be_cloned(src: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::symlink_metadata(src) {
        Ok(meta) => meta.permissions().mode() & 0o077 == 0,
        Err(_) => true,
    }
}

#[cfg(target_os = "macos")]
fn clone_step(src: &Path, dst: &Path, _options: &CloneOptions) -> StepResult {
    if source_must_not_be_cloned(src) {
        return StepResult::Unavailable;
    }
    match clone_file_nofollow(src, dst) {
        Ok(()) => StepResult::Done,
        Err(CloneError::Io { source, .. }) if source.kind() == io::ErrorKind::AlreadyExists => {
            StepResult::Exists
        }
        // Not APFS, another volume, or any other refusal: the next step decides.
        Err(_) => StepResult::Unavailable,
    }
}

/// Linux `FICLONE` (Btrfs, XFS with reflinks, bcachefs, ...). The ioctl needs the destination to
/// exist, so it runs on the temp file of the atomic-write path and is published exclusively, never
/// visible half-made. Only architectures whose `_IOW` encoding the constant below matches.
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64",
        target_arch = "s390x"
    )
))]
fn clone_step(src: &Path, dst: &Path, options: &CloneOptions) -> StepResult {
    use crate::atomic::AtomicWriter;
    use std::os::fd::AsRawFd;
    /// `FICLONE` = `_IOW(0x94, 9, int)`.
    const FICLONE: libc::c_ulong = 0x4004_9409;

    let Ok(input) = open_source(src) else {
        return StepResult::Unavailable;
    };
    // The temp is created `0600`: the ioctl makes the source's bytes readable through it at once,
    // so it must not start wider than a private source. It gets the source's mode before it is
    // published, as a macOS clone has it.
    let write_options = AtomicWriteOptions {
        replace: ReplaceMode::NoReplace,
        durable: options.durable,
        privacy: Privacy::OwnerOnlyUnixInheritWindows,
        ..AtomicWriteOptions::default()
    };
    let mut writer = match AtomicWriter::create_with(dst, write_options) {
        Ok(writer) => writer,
        Err(_) => return StepResult::Unavailable,
    };
    // SAFETY: both descriptors are open for the call; FICLONE takes the source descriptor as its
    // argument and reads no memory.
    let rc = unsafe {
        libc::ioctl(
            writer.file_mut().as_raw_fd(),
            FICLONE as _,
            input.as_raw_fd(),
        )
    };
    if rc != 0 {
        // EOPNOTSUPP, EXDEV, EINVAL, ENOTTY...: no clone here. Dropping the writer removes the temp.
        return StepResult::Unavailable;
    }
    let moded = input
        .metadata()
        .and_then(|meta| writer.file_mut().set_permissions(meta.permissions()));
    if moded.is_err() {
        return StepResult::Unavailable;
    }
    match writer.commit() {
        Ok(()) => StepResult::Done,
        Err(FsError::AlreadyExists(_)) => StepResult::Exists,
        // Published, then a later step failed: report it, do not try another way to make `dst`.
        Err(e) if e.target_replaced() => StepResult::Failed(e),
        Err(_) => StepResult::Unavailable,
    }
}

/// Windows and every other target: no copy-on-write clone step; go on to the hard link.
#[cfg(not(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "arm",
            target_arch = "aarch64",
            target_arch = "riscv64",
            target_arch = "loongarch64",
            target_arch = "s390x"
        )
    )
)))]
fn clone_step(_src: &Path, _dst: &Path, _options: &CloneOptions) -> StepResult {
    StepResult::Unavailable
}

/// A clone or link is a new directory entry: sync the directory it lives in so the entry survives
/// a power loss (Unix; Windows has no directory flush, see [`crate::publication_durability`]).
fn finish(dst: &Path, options: &CloneOptions, outcome: CloneOutcome) -> FsResult<CloneOutcome> {
    if options.durable {
        crate::atomic::sync_parent_dir(dst).map_err(|source| FsError::DirSyncFailed {
            path: dst.to_path_buf(),
            source,
        })?;
    }
    Ok(outcome)
}

/// The byte-copy floor: read `src` (opened without following a link), write it through the
/// atomic-write path, publish exclusively.
///
/// Everything about the source that the copy depends on (that it is a regular file, and its
/// permission bits) is read from the **opened handle**, never from the path metadata that
/// [`check_source`] took earlier: the clone and link steps run in between, and a source replaced
/// in that gap (a `0644` file by a `0600` secret) must be copied with the permissions of the
/// bytes actually read, not those of the file that used to be there.
fn copy_step(src: &Path, dst: &Path, options: &CloneOptions) -> FsResult<()> {
    let mut input = open_source(src).map_err(|e| match e.raw_os_error() {
        #[cfg(unix)]
        Some(libc::ELOOP) => FsError::Symlink(src.to_path_buf()),
        _ => FsError::io(src, e),
    })?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let opened = input.metadata().map_err(|e| FsError::io(src, e))?;
        if opened.file_attributes() & REPARSE_POINT != 0 {
            return Err(FsError::Symlink(src.to_path_buf()));
        }
    }
    let opened = input.metadata().map_err(|e| FsError::io(src, e))?;
    if !opened.is_file() {
        return Err(FsError::NotAFile(src.to_path_buf()));
    }
    // Unless the caller asked for a fresh file the copy ends with the source's permission bits.
    // The temp must not start wider than that: a `0600` source copied under umask `022` would
    // otherwise sit in a `0644` temp while its bytes are written, readable by anyone who can
    // traverse the directory and open it in that window. So on Unix the temp is created `0600`
    // and given the source's bits at the end, before it is published. A fresh file ends with the
    // default mode anyway, so it is created the ordinary way. Windows keeps the parent's ACL.
    let write_options = AtomicWriteOptions {
        replace: ReplaceMode::NoReplace,
        durable: options.durable,
        privacy: if options.fresh_metadata {
            Privacy::Inherit
        } else {
            Privacy::OwnerOnlyUnixInheritWindows
        },
        ..AtomicWriteOptions::default()
    };
    atomic_write_with_options(dst, write_options, |out| {
        io::copy(&mut input, out)?;
        #[cfg(unix)]
        if !options.fresh_metadata {
            out.set_permissions(opened.permissions())?;
        }
        #[cfg(not(unix))]
        let _ = (&opened, options.fresh_metadata);
        Ok(())
    })
}

#[cfg(windows)]
const REPARSE_POINT: u32 = 0x400;

/// Opens `src` for reading without following a link at its name: `O_NOFOLLOW` on Unix, an
/// open-the-reparse-point-itself flag on Windows (the caller checks the attribute).
fn open_source(src: &Path) -> io::Result<File> {
    let mut open = OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        open.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    open.open(src)
}

/// The source must be a regular file, not a link to one. Returns its metadata (not followed).
fn check_source(src: &Path) -> FsResult<std::fs::Metadata> {
    let meta = std::fs::symlink_metadata(src).map_err(|e| FsError::io(src, e))?;
    if is_link(&meta) {
        return Err(FsError::Symlink(src.to_path_buf()));
    }
    if !meta.is_file() {
        return Err(FsError::NotAFile(src.to_path_buf()));
    }
    Ok(meta)
}

/// The destination name must be free. A link there (even dangling) is refused as a link, anything
/// else as already existing; neither is touched.
fn check_destination(dst: &Path) -> FsResult<()> {
    match std::fs::symlink_metadata(dst) {
        Ok(meta) if is_link(&meta) => Err(FsError::Symlink(dst.to_path_buf())),
        Ok(_) => Err(FsError::AlreadyExists(dst.to_path_buf())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(FsError::io(dst, e)),
    }
}

fn is_link(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// Step seam for tests (feature `testing`, off by default; debug builds only). Replaces the clone
/// or the hard-link primitive on the calling thread, so each [`CloneOutcome`] can be produced
/// deterministically on any filesystem.
#[cfg(feature = "testing")]
pub mod testing {
    use super::Step;
    use std::cell::RefCell;
    use std::io;
    use std::path::Path;

    type StepFn = Box<dyn Fn(&Path, &Path) -> io::Result<()>>;

    thread_local! {
        static CLONE: RefCell<Option<StepFn>> = const { RefCell::new(None) };
        static LINK: RefCell<Option<StepFn>> = const { RefCell::new(None) };
    }

    /// Removes the override when dropped.
    pub struct HookGuard(fn());

    impl Drop for HookGuard {
        fn drop(&mut self) {
            (self.0)();
        }
    }

    /// Stand in for the clone step on this thread until the guard drops. `Ok(())` means the
    /// closure created the destination itself (it is a clone); `Err` of kind `AlreadyExists` is a
    /// destination that appeared first; any other `Err` is "this step is unavailable here".
    pub fn override_clone(step: impl Fn(&Path, &Path) -> io::Result<()> + 'static) -> HookGuard {
        CLONE.with(|slot| *slot.borrow_mut() = Some(Box::new(step)));
        HookGuard(|| CLONE.with(|slot| *slot.borrow_mut() = None))
    }

    /// Stand in for the hard-link step, with the same meaning of the closure's result.
    pub fn override_link(step: impl Fn(&Path, &Path) -> io::Result<()> + 'static) -> HookGuard {
        LINK.with(|slot| *slot.borrow_mut() = Some(Box::new(step)));
        HookGuard(|| LINK.with(|slot| *slot.borrow_mut() = None))
    }

    pub(super) fn overridden(step: Step, src: &Path, dst: &Path) -> Option<io::Result<()>> {
        let slot = match step {
            Step::Clone => &CLONE,
            Step::Link => &LINK,
        };
        slot.with(|slot| slot.borrow().as_ref().map(|step| step(src, dst)))
    }
}
