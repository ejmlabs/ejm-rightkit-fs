//! Native bindings match winternl.h/winbase.h. No path is retained for operations.
use super::{DirEntry, EntryKind, EntryStat, StateOptions};
use crate::atomic::{SyncLevel, SyncPolicy};
use crate::error::{FsError, Result};
use crate::private::{discard_unverified_owner_only, verify_owner_only_handle, SecurityDescriptor};
use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr::{null, null_mut};
use std::sync::Mutex;

type Handle = *mut c_void;
const READ_ATTRIBUTES: u32 = 0x80;
const READ_CONTROL: u32 = 0x0002_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const DELETE: u32 = 0x0001_0000;
// Files are opened without FILE_SHARE_DELETE on purpose: removal and publication go through
// handle-based disposition and rename, so nobody needs delete sharing on a file.
const SHARE_READ_WRITE: u32 = 3;
const FILE_SHARE_READ: u32 = 1;
// Directories default to the same, so a pinned directory (and every ancestor) cannot be renamed
// or deleted under the pin, and `StateDir::path()` stays true. `allow_rename_while_open`
// switches directories to this.
const SHARE_READ_WRITE_DELETE: u32 = 7;
// FILE_WRITE_THROUGH on the handle that creates a directory, renames or deletes: the same
// mechanism MOVEFILE_WRITE_THROUGH relies on. There is no directory flush on Windows (see
// `Dir::sync`).
const FILE_WRITE_THROUGH: u32 = 0x2;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_OPEN_IF: u32 = 3;
const DIRECTORY_FILE: u32 = 1;
const NON_DIRECTORY_FILE: u32 = 0x40;
const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const ATTRIBUTE_DIRECTORY: u32 = 0x10;
const ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_TAG_INFO: i32 = 9;
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const FILE_DISPOSITION_INFO: i32 = 4;
const FILE_RENAME_INFORMATION: i32 = 10;
const FILE_ID_BOTH_DIRECTORY_INFO: i32 = 10;
const FILE_ID_BOTH_DIRECTORY_RESTART_INFO: i32 = 11;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: Handle,
    object_name: *mut UnicodeString,
    attributes: u32,
    security_descriptor: *mut c_void,
    security_quality_of_service: *mut c_void,
}

#[repr(C)]
struct IoStatusBlock {
    status: usize, // NTSTATUS / pointer union.
    information: usize,
}

#[repr(C)]
struct AttributeTagInfo {
    attributes: u32,
    tag: u32,
}

#[repr(C)]
#[derive(PartialEq, Eq)]
struct FileIdInfo {
    volume: u64,
    id: [u8; 16],
}

#[repr(C)]
struct RenameInfo {
    replace_if_exists: u8,
    root_directory: Handle,
    file_name_length: u32,
    file_name: [u16; 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DirectoryInfo {
    next_entry_offset: u32,
    file_index: u32,
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    end_of_file: i64,
    allocation_size: i64,
    attributes: u32,
    file_name_length: u32,
    ea_size: u32,
    short_name_length: i8,
    short_name: [u16; 12],
    file_id: i64,
    file_name: [u16; 1],
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        file: *mut Handle,
        access: u32,
        attributes: *mut ObjectAttributes,
        status: *mut IoStatusBlock,
        allocation: *const i64,
        file_attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_length: u32,
    ) -> i32;
    fn NtSetInformationFile(
        file: Handle,
        status: *mut IoStatusBlock,
        info: *const c_void,
        size: u32,
        class: i32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
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
        template: Handle,
    ) -> Handle;
    fn GetFileInformationByHandleEx(file: Handle, class: i32, info: *mut c_void, size: u32) -> i32;
    fn SetFileInformationByHandle(file: Handle, class: i32, info: *const c_void, size: u32) -> i32;
    // Only the tests read security descriptors back and free them.
    #[cfg_attr(not(test), allow(dead_code))]
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}

/// What a pinned directory remembers from its [`StateOptions`], inherited by children.
#[derive(Clone, Copy, Debug)]
struct Config {
    /// Share mode for directory handles (`SHARE_READ_WRITE` or `SHARE_READ_WRITE_DELETE`).
    directory_share: u32,
    /// Create without an explicit security descriptor, so the parent's ACL is inherited.
    inherit_acl: bool,
}

impl Config {
    fn from_options(options: &StateOptions) -> Self {
        Self {
            directory_share: if options.allow_rename_while_open {
                SHARE_READ_WRITE_DELETE
            } else {
                SHARE_READ_WRITE
            },
            inherit_acl: options.inherit_acl(),
        }
    }
}

#[derive(Debug)]
pub struct Dir {
    file: File,
    enumeration: Mutex<()>,
    config: Config,
}

fn raw_attributes(file: &File, shown: &Path) -> Result<AttributeTagInfo> {
    let mut info = AttributeTagInfo {
        attributes: 0,
        tag: 0,
    };
    // SAFETY: open handle and correctly sized writable structure.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_ATTRIBUTE_TAG_INFO,
            (&mut info as *mut AttributeTagInfo).cast(),
            size_of::<AttributeTagInfo>() as u32,
        )
    } == 0
    {
        return Err(FsError::io(shown, io::Error::last_os_error()));
    }
    Ok(info)
}

/// The attributes of an opened child, refusing any reparse point.
fn attributes(file: &File, shown: &Path) -> Result<u32> {
    let info = raw_attributes(file, shown)?;
    if info.attributes & ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(FsError::Symlink(shown.to_path_buf()));
    }
    Ok(info.attributes)
}

/// The attributes of an opened child that must be a symlink or a junction (a mount point): the one
/// kind of reparse point [`Dir::remove_link`] may delete. Anything else is refused.
fn link_attributes(file: &File, shown: &Path) -> Result<u32> {
    let info = raw_attributes(file, shown)?;
    let is_link = info.attributes & ATTRIBUTE_REPARSE_POINT != 0
        && matches!(
            info.tag,
            IO_REPARSE_TAG_SYMLINK | IO_REPARSE_TAG_MOUNT_POINT
        );
    if !is_link {
        return Err(FsError::io(
            shown,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the entry is not a symlink or junction",
            ),
        ));
    }
    Ok(info.attributes)
}

/// What an opened child must be: an ordinary entry, or the reparse point itself.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    /// Refuse a reparse point (every ordinary child open).
    NoReparse,
    /// Require a symlink or junction and keep it open as the link itself (link removal).
    ReparseLink,
}

/// Mark an open child (opened with `DELETE`) for deletion through that handle.
fn mark_delete(file: &File, name: &str) -> Result<()> {
    let delete: u8 = 1; // FILE_DISPOSITION_INFO.DeleteFile is BOOLEAN, not BOOL.
                        // SAFETY: live child handle, opened without reparse traversal or delete sharing.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FILE_DISPOSITION_INFO,
            (&delete as *const u8).cast(),
            1,
        )
    } == 0
    {
        return Err(FsError::io(name, io::Error::last_os_error()));
    }
    Ok(())
}

pub(super) fn valid_component(name: &str, limit: usize) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !name
            .chars()
            .any(|c| c < ' ' || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        && name.encode_utf16().count() <= limit
}

fn component(name: &OsStr) -> Result<Vec<u16>> {
    let shown = name.to_string_lossy();
    // Lossy names are refused rather than addressing a different entry.
    let text = name
        .to_str()
        .ok_or_else(|| FsError::BadName(shown.into_owned()))?;
    if !valid_component(text, super::PLATFORM_NAME_LIMIT) {
        return Err(FsError::BadName(text.to_string()));
    }
    Ok(name.encode_wide().collect())
}

impl Dir {
    fn open_path(path: &Path, config: Config, share: u32) -> Result<Self> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        if wide[..wide.len() - 1].contains(&0) {
            return Err(FsError::BadName(path.display().to_string()));
        }
        // SAFETY: NUL-terminated path, no security/template pointers. OPEN_REPARSE_POINT
        // opens the leaf itself; identity is compared with the component walk before use.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                1 | READ_ATTRIBUTES | SYNCHRONIZE,
                share,
                null(),
                3,
                0x0200_0000 | 0x0020_0000,
                null_mut(),
            ) // FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
        };
        if raw as isize == -1 || raw.is_null() {
            return Err(FsError::io(path, io::Error::last_os_error()));
        }
        // SAFETY: CreateFileW returned a fresh owned handle.
        Self::verified(unsafe { File::from_raw_handle(raw) }, path, config)
    }

    fn identity(&self, shown: &Path) -> Result<FileIdInfo> {
        let mut id = FileIdInfo {
            volume: 0,
            id: [0; 16],
        };
        // SAFETY: live directory handle and a correctly sized FileIdInfo out-parameter.
        if unsafe {
            GetFileInformationByHandleEx(
                self.file.as_raw_handle(),
                18,
                (&mut id as *mut FileIdInfo).cast(),
                size_of::<FileIdInfo>() as u32,
            )
        } == 0
        {
            return Err(FsError::io(shown, io::Error::last_os_error()));
        }
        Ok(id)
    }

    fn verified(file: File, shown: &Path, config: Config) -> Result<Self> {
        if attributes(&file, shown)? & ATTRIBUTE_DIRECTORY == 0 {
            return Err(FsError::NotADirectory(shown.to_path_buf()));
        }
        Ok(Self {
            file,
            enumeration: Mutex::new(()),
            config,
        })
    }

    fn open_at(
        &self,
        entry_name: &OsStr,
        access: u32,
        disposition: u32,
        options: u32,
    ) -> Result<File> {
        self.open_at_with(
            entry_name,
            access,
            disposition,
            options,
            SHARE_READ_WRITE,
            Check::NoReparse,
        )
    }

    /// [`open_at`](Self::open_at) with the file share mode (directories always use the configured
    /// directory share) and what the opened child must be.
    fn open_at_with(
        &self,
        entry_name: &OsStr,
        access: u32,
        disposition: u32,
        options: u32,
        share_file: u32,
        check: Check,
    ) -> Result<File> {
        self.open_at_with_retention(
            entry_name,
            access,
            disposition,
            options,
            share_file,
            check,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn open_at_with_retention(
        &self,
        entry_name: &OsStr,
        access: u32,
        disposition: u32,
        options: u32,
        share_file: u32,
        check: Check,
        retain_name: bool,
    ) -> Result<File> {
        let shown = Path::new(entry_name);
        let mut wide = component(entry_name)?;
        let len = u16::try_from(wide.len() * 2)
            .map_err(|_| FsError::BadName(shown.display().to_string()))?;
        let mut name = UnicodeString {
            length: len,
            maximum_length: len,
            buffer: wide.as_mut_ptr(),
        };
        let directory = options & DIRECTORY_FILE != 0;
        // `FILE_OPEN_IF` cannot request verification/cleanup rights from an already-existing
        // ancestor. Create privately first; an actual collision reopens with original minimal
        // rights. A collision followed by disappearance is an error, never a broad retry.
        let create_only = disposition == FILE_OPEN_IF && !self.config.inherit_acl;
        let native_disposition = if create_only {
            FILE_CREATE
        } else {
            disposition
        };
        let security = if native_disposition != FILE_OPEN && !self.config.inherit_acl {
            Some(SecurityDescriptor::owner_only(shown, directory)?)
        } else {
            None
        };
        // A temporary creation needs DELETE for failed-verification cleanup. A permanent lock
        // must keep its name on error and omit DELETE: other StateDir readers intentionally do
        // not share deletion, and must be able to open while the creator retains its handle.
        let creation_security_access = if security.is_some() {
            READ_CONTROL | if retain_name { 0 } else { DELETE }
        } else {
            0
        };
        let mut object = ObjectAttributes {
            length: size_of::<ObjectAttributes>() as u32,
            root_directory: self.file.as_raw_handle(),
            object_name: &mut name,
            attributes: 0x40, // OBJ_CASE_INSENSITIVE, never OBJ_INHERIT.
            security_descriptor: security.as_ref().map_or(null_mut(), |s| s.0),
            security_quality_of_service: null_mut(),
        };
        let mut status = IoStatusBlock {
            status: 0,
            information: 0,
        };
        let mut raw = null_mut();
        // SAFETY: live root handle, bounded single component, valid structures; synchronous
        // open finishes before any buffers or security descriptor are released.
        let result = unsafe {
            NtCreateFile(
                &mut raw,
                access | READ_ATTRIBUTES | SYNCHRONIZE | creation_security_access,
                &mut object,
                &mut status,
                null(),
                0x80,
                if directory {
                    self.config.directory_share
                } else {
                    share_file
                },
                native_disposition,
                options | OPEN_REPARSE_POINT | SYNCHRONOUS_IO_NONALERT,
                null(),
                0,
            )
        };
        if result < 0 {
            // SAFETY: translates NTSTATUS, not thread-local GetLastError.
            let error =
                io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(result) } as i32);
            if create_only && error.kind() == io::ErrorKind::AlreadyExists {
                return self.open_at_with_retention(
                    entry_name,
                    access,
                    FILE_OPEN,
                    options,
                    share_file,
                    check,
                    retain_name,
                );
            }
            return Err(if error.kind() == io::ErrorKind::AlreadyExists {
                FsError::AlreadyExists(shown.to_path_buf())
            } else {
                FsError::io(shown, error)
            });
        }
        if raw.is_null() || raw as isize == -1 {
            return Err(FsError::Unavailable(
                "NtCreateFile returned no child handle",
            ));
        }
        const FILE_CREATED: usize = 2;
        // SAFETY: successful NtCreateFile transfers ownership of this handle.
        let file = unsafe { File::from_raw_handle(raw) };
        #[cfg(feature = "testing")]
        if retain_name {
            super::testing::after_empty_create(&file)?;
        }
        let checked = match check {
            Check::NoReparse => attributes(&file, shown),
            Check::ReparseLink => link_attributes(&file, shown),
        };
        if let Err(error) = checked {
            if !retain_name && security.is_some() && status.information == FILE_CREATED {
                let _ = discard_unverified_owner_only(&file, shown);
            }
            return Err(error);
        }
        // A descriptor is only supplied for FILE_CREATE, so this handle is new. Failed
        // temporary entries are disposed; a visible final lock name is never removed here.
        if security.is_some() {
            if status.information != FILE_CREATED {
                if !retain_name {
                    let _ = discard_unverified_owner_only(&file, shown);
                }
                return Err(FsError::Unavailable(
                    "NtCreateFile did not report a new private entry",
                ));
            }
            // Direct permanent creation verifies this same handle in finish_read_open, once,
            // with the same strict predicate as an opted-in later reader.
            if !retain_name {
                if let Err(error) = verify_owner_only_handle(&file, shown) {
                    let _ = discard_unverified_owner_only(&file, shown);
                    return Err(error);
                }
            }
        }
        Ok(file)
    }

    /// `_shared` is the unix "do not require a private directory" policy; Windows checks no ACL or
    /// mode on an existing directory, so it has no effect here.
    pub fn pin(path: &Path, create: bool, options: &StateOptions, _shared: bool) -> Result<Self> {
        let config = Config::from_options(options);
        if path.as_os_str().is_empty() {
            return Err(FsError::BadName(String::new()));
        }
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| FsError::io(path, e))?
                .join(path)
        };
        let mut parts = absolute.components();
        let mut root = PathBuf::new();
        match parts.next() {
            Some(Component::Prefix(p))
                if matches!(
                    p.kind(),
                    Prefix::Disk(_)
                        | Prefix::VerbatimDisk(_)
                        | Prefix::UNC(_, _)
                        | Prefix::VerbatimUNC(_, _)
                ) =>
            {
                root.push(p.as_os_str())
            }
            _ => return Err(FsError::BadName(path.display().to_string())),
        }
        if !matches!(parts.next(), Some(Component::RootDir)) {
            return Err(FsError::BadName(path.display().to_string()));
        }
        root.push("\\");
        let mut current = Self::open_path(&root, config, config.directory_share)?;
        for part in parts {
            match part {
                Component::CurDir => continue,
                Component::Normal(name) => {
                    root.push(name);
                    // A directory this walk creates is opened write-through, like `child`: the
                    // creation is committed before the walk descends into it. Windows has no
                    // directory flush to add (see `Dir::sync`), so this is the strongest
                    // equivalent of publishing each new ancestor.
                    let file = current.open_at(
                        name,
                        1,
                        if create { FILE_OPEN_IF } else { FILE_OPEN },
                        if create {
                            DIRECTORY_FILE | FILE_WRITE_THROUGH
                        } else {
                            DIRECTORY_FILE
                        },
                    )?;
                    current = Self::verified(file, &root, config)?;
                }
                _ => return Err(FsError::BadName(path.display().to_string())),
            }
        }
        // Retain one CreateFileW handle for the state directory, checked against the
        // no-reparse component walk. Any ancestor swap during this final open fails closed.
        // The walk's creation handle may carry DELETE access (private-descriptor cleanup rights),
        // which would make this open a sharing violation without FILE_SHARE_DELETE. Record its
        // identity and release it first; the identity comparison below still fails closed on
        // any swap in between.
        let walked = current.identity(path)?;
        drop(current);
        let pinned = Self::open_path(&absolute, config, config.directory_share)?;
        if walked != pinned.identity(path)? {
            return Err(FsError::IdentityChanged(path.to_path_buf()));
        }
        Ok(pinned)
    }

    pub fn child(&self, name: &str, _policy: SyncPolicy, _shared: bool) -> Result<Self> {
        // Created write-through: the directory entry is committed with the creation.
        let file = self.open_at(
            OsStr::new(name),
            1,
            FILE_OPEN_IF,
            DIRECTORY_FILE | FILE_WRITE_THROUGH,
        )?;
        Self::verified(file, Path::new(name), self.config)
    }

    pub fn stat(&self, name: &str) -> Result<Option<EntryStat>> {
        let file = match self.open_at(OsStr::new(name), READ_ATTRIBUTES, FILE_OPEN, 0) {
            Ok(file) => file,
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e),
        };
        let m = file.metadata().map_err(|e| FsError::io(name, e))?;
        // The volume serial number and file index of the opened entry, so two names for one file
        // (or one file seen twice) compare equal as they do on unix.
        let key = crate::file_key::FileKey::of_file(&file, Path::new(name))?;
        Ok(Some(EntryStat {
            kind: if m.is_dir() {
                EntryKind::Dir
            } else if m.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            },
            size: m.len(),
            dev: key.volume,
            ino: key.index,
            mode: 0,
            modified_unix_ms: m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }))
    }

    /// Open a regular child for reading. `share_read_only` opens with `FILE_SHARE_READ` alone, so no
    /// other handle may write, delete or rename the file while this one is open (and the open fails
    /// with a sharing violation while somebody holds it for write).
    /// `require_owner_only` adds READ_CONTROL for the subsequent check on this exact handle.
    /// Ordinary pinned reads retain their existing access mask.
    pub fn open_read_with(&self, name: &str, options: &crate::handle::ReadOptions) -> Result<File> {
        self.open_at_with(
            OsStr::new(name),
            1 | if options.require_owner_only {
                READ_CONTROL
            } else {
                0
            },
            FILE_OPEN,
            NON_DIRECTORY_FILE,
            if options.share_read_only {
                FILE_SHARE_READ
            } else {
                SHARE_READ_WRITE
            },
            Check::NoReparse,
        )
    }

    pub fn create_new(&self, name: &str, retain_name: bool) -> Result<File> {
        self.open_at_with_retention(
            OsStr::new(name),
            0x0012_0116, // FILE_GENERIC_WRITE
            FILE_CREATE,
            NON_DIRECTORY_FILE,
            SHARE_READ_WRITE,
            Check::NoReparse,
            retain_name,
        )
    }

    fn rename(&self, from: &str, to: &str, replace: bool) -> Result<()> {
        let wide = component(OsStr::new(to))?;
        // Refuse an observed reparse destination. If it races this check, the kernel rename
        // replaces the directory entry itself; it never opens or writes through the link.
        if replace {
            if let Some(st) = self.stat(to)? {
                if st.kind != EntryKind::File {
                    return Err(FsError::NotAFile(to.into()));
                }
            }
        }
        // Write-through on the renamed handle, as MOVEFILE_WRITE_THROUGH does.
        let source = self.open_at(
            OsStr::new(from),
            DELETE,
            FILE_OPEN,
            NON_DIRECTORY_FILE | FILE_WRITE_THROUGH,
        )?;
        let offset = offset_of!(RenameInfo, file_name);
        let bytes = size_of::<RenameInfo>() + wide.len() * 2;
        let mut buffer = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        let info = buffer.as_mut_ptr().cast::<RenameInfo>();
        // SAFETY: aligned allocation covers the header and complete UTF-16 name.
        unsafe {
            (*info).replace_if_exists = u8::from(replace);
            (*info).root_directory = self.file.as_raw_handle();
            (*info).file_name_length = (wide.len() * 2) as u32;
            std::ptr::copy_nonoverlapping(
                wide.as_ptr(),
                buffer.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
                wide.len(),
            );
            let mut status = IoStatusBlock {
                status: 0,
                information: 0,
            };
            // The native rename API resolves this single component against RootDirectory.
            // Keep the pinned handle rather than asking Win32 to translate a DOS path.
            let result = NtSetInformationFile(
                source.as_raw_handle(),
                &mut status,
                info.cast(),
                bytes as u32,
                FILE_RENAME_INFORMATION,
            );
            if result < 0 {
                let e = io::Error::from_raw_os_error(RtlNtStatusToDosError(result) as i32);
                return Err(if e.kind() == io::ErrorKind::AlreadyExists {
                    FsError::AlreadyExists(to.into())
                } else {
                    FsError::io(to, e)
                });
            }
        }
        Ok(())
    }

    pub fn rename_replace(&self, from: &str, to: &str) -> Result<()> {
        self.rename(from, to, true)
    }

    pub fn link_new(&self, from: &str, to: &str) -> Result<()> {
        // Windows publishes by atomic no-replace rename rather than a hard link. The
        // caller's subsequent temp unlink sees NotFound, which it already ignores.
        self.rename(from, to, false)
    }

    pub fn unlink(&self, name: &str) -> Result<()> {
        let file = self.open_at(
            OsStr::new(name),
            DELETE,
            FILE_OPEN,
            NON_DIRECTORY_FILE | FILE_WRITE_THROUGH,
        )?;
        mark_delete(&file, name)
    }

    /// Remove an empty directory child through a handle opened without reparse traversal, so a
    /// junction or symlink at `name` is refused ([`FsError::Symlink`]) and never followed. A directory
    /// that still holds entries fails with the OS error (`ERROR_DIR_NOT_EMPTY`).
    pub fn remove_dir(&self, name: &str) -> Result<()> {
        let dir = self.open_at(
            OsStr::new(name),
            DELETE,
            FILE_OPEN,
            DIRECTORY_FILE | FILE_WRITE_THROUGH,
        )?;
        mark_delete(&dir, name)
    }

    /// Remove a symlink or junction child itself: the entry is opened as the reparse point
    /// (`FILE_OPEN_REPARSE_POINT`) and deleted through that handle, so its target is never opened,
    /// touched or removed. Anything that is not a symlink or junction is refused as `InvalidInput`.
    pub fn remove_link(&self, name: &str) -> Result<()> {
        let link = self.open_at_with(
            OsStr::new(name),
            DELETE,
            FILE_OPEN,
            FILE_WRITE_THROUGH,
            SHARE_READ_WRITE_DELETE,
            Check::ReparseLink,
        )?;
        mark_delete(&link, name)
    }

    /// There is no directory flush on Windows, so this does nothing and says so (`None`): the
    /// pinned handle is opened for listing, and `FlushFileBuffers` on it fails with
    /// `ERROR_ACCESS_DENIED` (it needs `GENERIC_WRITE`; Pulse saw the same on NTFS). A directory
    /// handle opened for write would only be flushing NTFS metadata the filesystem journals
    /// anyway. Directory-entry durability comes from the operations themselves: the file is
    /// flushed before publication, and directory creation, the publishing rename and the
    /// removing delete are performed on handles opened `FILE_WRITE_THROUGH` (the mechanism
    /// `MOVEFILE_WRITE_THROUGH` uses), so they commit with the call. A power-loss test has not
    /// been run against that claim.
    pub fn sync(&self, _policy: SyncPolicy) -> io::Result<Option<SyncLevel>> {
        Ok(None)
    }

    pub fn entries(&self, limit: usize) -> Result<Vec<DirEntry>> {
        let _guard = self
            .enumeration
            .lock()
            .map_err(|_| FsError::Unavailable("directory enumeration lock poisoned"))?;
        // FILE_ID_BOTH_DIR_INFO requires 8-byte alignment, including on 32-bit Windows.
        let mut buffer = vec![0u64; 8192];
        let capacity = buffer.len() * size_of::<u64>();
        let mut class = FILE_ID_BOTH_DIRECTORY_RESTART_INFO;
        let mut out = Vec::new();
        loop {
            buffer.fill(0);
            // SAFETY: open directory and writable, aligned 64 KiB buffer.
            if unsafe {
                GetFileInformationByHandleEx(
                    self.file.as_raw_handle(),
                    class,
                    buffer.as_mut_ptr().cast(),
                    capacity as u32,
                )
            } == 0
            {
                let e = io::Error::last_os_error();
                return if e.raw_os_error() == Some(18) {
                    // ERROR_NO_MORE_FILES
                    Ok(out)
                } else {
                    Err(FsError::io(".", e))
                };
            }
            class = FILE_ID_BOTH_DIRECTORY_INFO;
            let mut offset = 0usize;
            loop {
                if offset > capacity - size_of::<DirectoryInfo>() {
                    return Err(FsError::Unavailable(
                        "malformed directory information header",
                    ));
                }
                // SAFETY: bounded header in initialized allocation.
                let info = unsafe {
                    std::ptr::read_unaligned(
                        buffer
                            .as_ptr()
                            .cast::<u8>()
                            .add(offset)
                            .cast::<DirectoryInfo>(),
                    )
                };
                let start = offset + offset_of!(DirectoryInfo, file_name);
                let len = info.file_name_length as usize;
                if len == 0 || !len.is_multiple_of(2) || len > capacity - start {
                    return Err(FsError::Unavailable("malformed directory information name"));
                }
                // SAFETY: bounded even-sized UTF-16 slice, aligned by directory record ABI.
                let name = unsafe {
                    std::slice::from_raw_parts(
                        buffer.as_ptr().cast::<u8>().add(start).cast::<u16>(),
                        len / 2,
                    )
                };
                // A name with unpaired surrogates comes back lossily (it cannot be addressed) rather
                // than failing the whole listing.
                let name = String::from_utf16_lossy(name);
                if name != "." && name != ".." {
                    if out.len() >= limit {
                        return Err(FsError::TooManyEntries(limit));
                    }
                    out.push(DirEntry {
                        name,
                        kind: super::kind_from_attributes(info.attributes),
                    });
                }
                let next = info.next_entry_offset as usize;
                if next == 0 {
                    break;
                }
                if !next.is_multiple_of(8)
                    || next < offset_of!(DirectoryInfo, file_name) + len
                    || next > capacity - offset
                {
                    return Err(FsError::Unavailable(
                        "malformed directory information offset",
                    ));
                }
                offset += next;
            }
        }
    }

    /// Reclaim only generated temps whose enumerated full file identity still matches the
    /// opened no-reparse handle. Public listings keep using `FILE_ID_BOTH_DIR_INFO`; cleanup uses
    /// its private extended-ID path so it never truncates an identity to 64 bits.
    pub(super) fn sweep_temps(&self) -> Result<usize> {
        let _guard = self
            .enumeration
            .lock()
            .map_err(|_| FsError::Unavailable("directory enumeration lock poisoned"))?;
        crate::windows_temp_sweep::sweep(&self.file)
    }
}

#[cfg(all(test, windows))]
#[path = "windows_tests.rs"]
mod tests;
