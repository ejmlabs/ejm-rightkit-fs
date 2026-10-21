//! Windows-only identity-pinned cleanup for generated temporary files.

use crate::atomic::{confirmed_dead_writer, generated_temp_pid};
use crate::error::{FsError, Result};
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

type Handle = *mut c_void;

const FILE_LIST_DIRECTORY: u32 = 0x0001;
const READ_ATTRIBUTES: u32 = 0x0080;
const DELETE: u32 = 0x0001_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const SHARE_READ_WRITE: u32 = 3;
const SHARE_READ_WRITE_DELETE: u32 = 7;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_OPEN: u32 = 1;
const NON_DIRECTORY_FILE: u32 = 0x40;
const SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_TAG_INFO: i32 = 9;
const FILE_ID_INFO: i32 = 18;
const FILE_ID_EXTD_DIRECTORY_INFO: i32 = 19;
const FILE_ID_EXTD_DIRECTORY_RESTART_INFO: i32 = 20;
const FILE_DISPOSITION_INFO: i32 = 4;
const ERROR_NO_MORE_FILES: i32 = 18;

#[repr(C)]
struct AttributeTagInfo {
    attributes: u32,
    reparse_tag: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FileIdentity {
    volume: u64,
    file_id: [u8; 16],
}

impl FileIdentity {
    fn supported(&self) -> bool {
        // Windows uses all-ones when a unique file ID is unavailable.
        self.file_id.iter().any(|byte| *byte != 0)
            && self.file_id.iter().any(|byte| *byte != u8::MAX)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ExtendedDirectoryInfo {
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
    reparse_tag: u32,
    file_id: [u8; 16],
    file_name: [u16; 1],
}

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
    status: usize,
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
        template: Handle,
    ) -> Handle;
    fn GetFileInformationByHandleEx(file: Handle, class: i32, info: *mut c_void, size: u32) -> i32;
    fn SetFileInformationByHandle(file: Handle, class: i32, info: *const c_void, size: u32) -> i32;
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
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

pub(crate) struct Candidate {
    pub(crate) name: String,
    identity: FileIdentity,
}

fn open_directory(path: &Path) -> Result<File> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if wide[..wide.len() - 1].contains(&0) {
        return Err(FsError::BadName(path.display().to_string()));
    }
    // This is an ordinary directory pin: it follows the supplied directory path as
    // `read_dir` does and does not inspect or change owner/DACL policy.
    // SAFETY: NUL-terminated path and no retained security/template pointers.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY | READ_ATTRIBUTES | SYNCHRONIZE,
            SHARE_READ_WRITE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if raw.is_null() || raw as isize == -1 {
        return Err(FsError::io(path, io::Error::last_os_error()));
    }
    // SAFETY: CreateFileW returned a fresh owned handle.
    let file = unsafe { File::from_raw_handle(raw) };
    if attributes(&file).map_err(|error| FsError::io(path, error))? & FILE_ATTRIBUTE_DIRECTORY == 0
    {
        return Err(FsError::NotADirectory(path.to_path_buf()));
    }
    Ok(file)
}

fn attributes(file: &File) -> io::Result<u32> {
    let mut info = AttributeTagInfo {
        attributes: 0,
        reparse_tag: 0,
    };
    // SAFETY: live handle and correctly sized output structure.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_ATTRIBUTE_TAG_INFO,
            (&mut info as *mut AttributeTagInfo).cast(),
            size_of::<AttributeTagInfo>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(info.attributes)
}

fn identity(file: &File) -> io::Result<FileIdentity> {
    let mut result = FileIdentity {
        volume: 0,
        file_id: [0; 16],
    };
    // SAFETY: live handle and correctly sized FILE_ID_INFO output structure.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_ID_INFO,
            (&mut result as *mut FileIdentity).cast(),
            size_of::<FileIdentity>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(result)
}

fn for_each_candidate(directory: &File, mut visit: impl FnMut(Candidate)) -> Result<()> {
    // FILE_ID_EXTD_DIR_INFO is available on the NTFS/ReFS targets supported here. If a
    // filesystem cannot supply full identities, fail closed rather than truncating to 64 bits.
    let mut buffer = vec![0u64; 8192];
    let capacity = buffer.len() * size_of::<u64>();
    let mut class = FILE_ID_EXTD_DIRECTORY_RESTART_INFO;
    let volume = identity(directory)
        .map_err(|error| FsError::io(".", error))?
        .volume;
    loop {
        buffer.fill(0);
        // SAFETY: live directory handle and aligned writable buffer of `capacity` bytes.
        if unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle(),
                class,
                buffer.as_mut_ptr().cast(),
                capacity as u32,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
                return Ok(());
            }
            return Err(FsError::io(".", error));
        }
        class = FILE_ID_EXTD_DIRECTORY_INFO;
        let mut offset = 0usize;
        loop {
            if offset > capacity - size_of::<ExtendedDirectoryInfo>() {
                return Err(FsError::Unavailable(
                    "malformed extended directory information header",
                ));
            }
            // SAFETY: bounded header in initialized storage.
            let info = unsafe {
                std::ptr::read_unaligned(
                    buffer
                        .as_ptr()
                        .cast::<u8>()
                        .add(offset)
                        .cast::<ExtendedDirectoryInfo>(),
                )
            };
            let name_start = offset + offset_of!(ExtendedDirectoryInfo, file_name);
            let name_len = info.file_name_length as usize;
            if name_len == 0 || !name_len.is_multiple_of(2) || name_len > capacity - name_start {
                return Err(FsError::Unavailable(
                    "malformed extended directory information name",
                ));
            }
            // SAFETY: bounded, even-sized UTF-16 slice.
            let name_wide = unsafe {
                std::slice::from_raw_parts(
                    buffer.as_ptr().cast::<u8>().add(name_start).cast::<u16>(),
                    name_len / 2,
                )
            };
            let name = String::from_utf16_lossy(name_wide);
            if name != "." && name != ".." {
                visit(Candidate {
                    name,
                    identity: FileIdentity {
                        volume,
                        file_id: info.file_id,
                    },
                });
            }
            let next = info.next_entry_offset as usize;
            if next == 0 {
                break;
            }
            if !next.is_multiple_of(8)
                || next < offset_of!(ExtendedDirectoryInfo, file_name) + name_len
                || next > capacity - offset
            {
                return Err(FsError::Unavailable(
                    "malformed extended directory information offset",
                ));
            }
            offset += next;
        }
    }
}

#[cfg(test)]
fn enumerate(directory: &File) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    for_each_candidate(directory, |candidate| out.push(candidate))?;
    Ok(out)
}

fn open_candidate(directory: &File, name: &str) -> io::Result<Option<File>> {
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    if wide.is_empty()
        || wide.len() > 255
        || wide.iter().any(|unit| matches!(*unit, 0 | 0x2f | 0x5c))
    {
        return Ok(None);
    }
    let length =
        u16::try_from(wide.len() * 2).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut unicode = UnicodeString {
        length,
        maximum_length: length,
        buffer: wide.as_mut_ptr(),
    };
    let mut object = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: directory.as_raw_handle(),
        object_name: &mut unicode,
        attributes: 0x40, // OBJ_CASE_INSENSITIVE; never OBJ_INHERIT.
        security_descriptor: null_mut(),
        security_quality_of_service: null_mut(),
    };
    let mut status = IoStatusBlock {
        status: 0,
        information: 0,
    };
    let mut raw = null_mut();
    // SAFETY: pinned directory handle, bounded single component, live object and status buffers.
    let result = unsafe {
        NtCreateFile(
            &mut raw,
            DELETE | READ_ATTRIBUTES | SYNCHRONIZE,
            &mut object,
            &mut status,
            null(),
            0,
            SHARE_READ_WRITE,
            FILE_OPEN,
            NON_DIRECTORY_FILE | OPEN_REPARSE_POINT | SYNCHRONOUS_IO_NONALERT,
            null(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    if raw.is_null() || raw as isize == -1 {
        return Err(io::Error::other(
            "NtCreateFile returned no candidate handle",
        ));
    }
    // SAFETY: successful NtCreateFile transfers ownership of this handle.
    Ok(Some(unsafe { File::from_raw_handle(raw) }))
}

fn open_if_same(directory: &File, candidate: &Candidate) -> io::Result<Option<File>> {
    if !candidate.identity.supported() {
        return Ok(None);
    }
    let Some(file) = open_candidate(directory, &candidate.name)? else {
        return Ok(None);
    };
    let attrs = attributes(&file)?;
    if attrs & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        return Ok(None);
    }
    let observed = identity(&file)?;
    if !observed.supported()
        || observed.volume != candidate.identity.volume
        || observed.file_id != candidate.identity.file_id
    {
        return Ok(None);
    }
    Ok(Some(file))
}

fn dispose(file: &File) -> io::Result<bool> {
    // The no-delete-share handle prevents a later rename/replacement between identity validation
    // and disposition. Callers perform the final dead-writer check immediately before this.
    let delete: u8 = 1;
    // SAFETY: same live handle whose full volume + 128-bit ID matched enumeration.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FILE_DISPOSITION_INFO,
            (&delete as *const u8).cast(),
            1,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

pub(crate) fn sweep(directory: &File) -> Result<usize> {
    let mut removed = 0;
    for_each_candidate(directory, |candidate| {
        let Some(pid) = generated_temp_pid(&candidate.name) else {
            return;
        };
        if !confirmed_dead_writer(pid) {
            return;
        }
        let file = match open_if_same(directory, &candidate) {
            Ok(Some(file)) => file,
            Ok(None) | Err(_) => return,
        };
        // Recheck after opening and identity comparison; disposition uses that retained handle.
        if confirmed_dead_writer(pid) && dispose(&file).unwrap_or(false) {
            removed += 1;
        }
    })?;
    Ok(removed)
}

pub(crate) fn sweep_path(path: &Path) -> Result<usize> {
    let directory = match open_directory(path) {
        Ok(directory) => directory,
        Err(error) if error.is_not_found() => return Ok(0),
        Err(error) => return Err(error),
    };
    sweep(&directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_after_enumeration_is_preserved_and_original_candidate_is_removable() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join(".x.rktmp-4294967294-1-0");
        let replacement = dir.path().join("replacement");
        std::fs::write(&old, b"old temp").unwrap();
        let directory = open_directory(dir.path()).unwrap();
        let candidate = enumerate(&directory)
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.name == ".x.rktmp-4294967294-1-0")
            .unwrap();
        assert!(candidate.identity.supported());

        std::fs::rename(&old, &replacement).unwrap();
        std::fs::write(&old, b"unrelated replacement").unwrap();
        assert!(open_if_same(&directory, &candidate).unwrap().is_none());
        assert_eq!(std::fs::read(&old).unwrap(), b"unrelated replacement");

        let replacement_candidate = enumerate(&directory)
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.name == ".x.rktmp-4294967294-1-0")
            .unwrap();
        let file = open_if_same(&directory, &replacement_candidate)
            .unwrap()
            .unwrap();
        assert!(dispose(&file).unwrap());
        // Disposition completes when the last handle closes; until then the entry is
        // delete-pending and may still be reported.
        drop(file);
        assert!(!old.exists());
        assert_eq!(std::fs::read(replacement).unwrap(), b"old temp");
    }
}
