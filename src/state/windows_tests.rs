use super::*;
use crate::StateDir;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::fs::{symlink_file, OpenOptionsExt};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn DeviceIoControl(
        device: Handle,
        code: u32,
        input: *const c_void,
        input_size: u32,
        output: *mut c_void,
        output_size: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
}

// Junctions need no symlink privilege. Still skip cleanly on unsupported filesystems/policy.
fn junction(link: &Path, target: &Path) -> io::Result<()> {
    let canonical = target.canonicalize()?;
    let print: Vec<u16> = canonical.as_os_str().encode_wide().collect();
    let mut substitute = print.clone();
    if !substitute.starts_with(&[92, 92, 63, 92]) {
        return Err(io::Error::other(
            "junction target is not an extended local path",
        ));
    }
    substitute[1] = 63; // \\?\ -> \??\ native namespace.
    let data_length = 8 + (substitute.len() + print.len() + 2) * 2;
    if data_length > 16_376 {
        return Err(io::Error::other(
            "junction target exceeds reparse buffer cap",
        ));
    }
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&0xa000_0003u32.to_le_bytes()); // IO_REPARSE_TAG_MOUNT_POINT
    buffer.extend_from_slice(&(data_length as u16).to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    for value in [
        0,
        (substitute.len() * 2) as u16,
        ((substitute.len() + 1) * 2) as u16,
        (print.len() * 2) as u16,
    ] {
        buffer.extend_from_slice(&value.to_le_bytes());
    }
    for unit in substitute
        .iter()
        .copied()
        .chain(Some(0))
        .chain(print.iter().copied())
        .chain(Some(0))
    {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    fs::create_dir(link)?;
    let result = (|| {
        let file = OpenOptions::new()
            .write(true)
            .share_mode(3)
            .custom_flags(0x0200_0000 | 0x0020_0000)
            .open(link)?;
        let mut returned = 0;
        // SAFETY: synchronous handle and bounded mount-point reparse buffer.
        if unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                0x0009_00a4,
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                null_mut(),
                0,
                &mut returned,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir(link);
    }
    result
}

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let guard = tempfile::tempdir().unwrap();
    let path = guard.path().canonicalize().unwrap();
    (guard, path)
}

#[test]
fn native_temp_creation_and_relative_publication() {
    let (_guard, root) = tmp();
    let state = StateDir::create(&root.join("state")).unwrap();
    let (temp, mut file) = state.create_temp("record").unwrap();
    file.write_all(b"inside").unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert_eq!(state.read(&temp, 100).unwrap(), b"inside");
    state.dir.rename_replace(&temp, "record").unwrap();
    assert!(!state.exists(&temp).unwrap());
    assert_eq!(state.read("record", 100).unwrap(), b"inside");
}

#[test]
fn ancestor_junction_swap_between_temp_writes_stays_pinned() {
    let (_guard, root) = tmp();
    let outside = root.join("outside");
    fs::create_dir_all(outside.join("state")).unwrap();
    fs::write(outside.join("state/keep"), b"outside").unwrap();
    let probe = root.join("probe");
    if let Err(e) = junction(&probe, &outside) {
        eprintln!("SKIP ancestor junction case: {e}");
        return;
    }
    fs::remove_dir(&probe).unwrap();
    let parent = root.join("parent");
    let moved = root.join("moved");
    let state = StateDir::create(&parent.join("state")).unwrap();
    state.write("keep", b"original").unwrap();
    let (temp, mut file) = state.create_temp("record").unwrap();
    file.write_all(b"before-").unwrap();
    // Deterministically attack after temp creation, before remaining bytes/publication.
    let swapped = fs::rename(&parent, &moved).is_ok();
    if swapped {
        if let Err(e) = junction(&parent, &outside) {
            drop(file);
            state.dir.unlink(&temp).unwrap();
            drop(state);
            fs::rename(&moved, &parent).unwrap();
            eprintln!("SKIP ancestor junction case: {e}");
            return;
        }
    }
    file.write_all(b"after").unwrap();
    file.sync_all().unwrap();
    drop(file);
    state.dir.rename_replace(&temp, "record").unwrap();
    state.write("later", b"pinned").unwrap();
    state.write_new("new", b"pinned").unwrap();
    let child = state.child("child").unwrap();
    child.write("nested", b"pinned").unwrap();
    assert_eq!(state.read("record", 100).unwrap(), b"before-after");
    assert!(state.remove("keep").unwrap());
    assert!(!state.remove("keep").unwrap());
    let original = if swapped {
        moved.join("state")
    } else {
        parent.join("state")
    };
    assert_eq!(fs::read(original.join("record")).unwrap(), b"before-after");
    assert_eq!(fs::read(original.join("later")).unwrap(), b"pinned");
    assert_eq!(fs::read(original.join("new")).unwrap(), b"pinned");
    assert_eq!(fs::read(original.join("child/nested")).unwrap(), b"pinned");
    assert_eq!(fs::read(outside.join("state/keep")).unwrap(), b"outside");
    assert_eq!(fs::read_dir(outside.join("state")).unwrap().count(), 1);
    let mut names = state.list(10).unwrap();
    names.sort();
    assert_eq!(names, ["child", "later", "new", "record"]);
    drop((child, state));
    if swapped {
        fs::remove_dir(&parent).unwrap();
    }
}

#[test]
fn pinned_directory_itself_cannot_be_renamed_or_removed() {
    let (_guard, root) = tmp();
    let path = root.join("state");
    let state = StateDir::create(&path).unwrap();
    assert!(fs::rename(&path, root.join("moved")).is_err());
    assert!(fs::remove_dir(&path).is_err());
    state.write("record", b"inside").unwrap();
    assert_eq!(fs::read(path.join("record")).unwrap(), b"inside");
    drop(state);
    fs::rename(&path, root.join("moved")).unwrap();
}

#[test]
fn target_symlink_swap_mid_write_is_refused() {
    let (_guard, root) = tmp();
    let outside = root.join("victim");
    fs::write(&outside, b"intact").unwrap();
    let state = StateDir::create(&root.join("state")).unwrap();
    state.write("record", b"old").unwrap();
    let (temp, mut file) = state.create_temp("record").unwrap();
    file.write_all(b"before-").unwrap();
    fs::remove_file(state.path().join("record")).unwrap();
    if let Err(e) = symlink_file(&outside, state.path().join("record")) {
        drop(file);
        state.dir.unlink(&temp).unwrap();
        eprintln!("SKIP file symlink case: {e}");
        return;
    }
    file.write_all(b"after").unwrap();
    drop(file);
    assert!(matches!(
        state.dir.rename_replace(&temp, "record"),
        Err(FsError::Symlink(_))
    ));
    state.dir.unlink(&temp).unwrap();
    assert!(matches!(
        state.write("record", b"attack"),
        Err(FsError::Symlink(_))
    ));
    assert!(state.write_new("record", b"attack").is_err());
    assert!(matches!(
        state.read("record", 100),
        Err(FsError::Symlink(_))
    ));
    assert!(matches!(state.remove("record"), Err(FsError::Symlink(_))));
    assert_eq!(fs::read(&outside).unwrap(), b"intact");
    assert!(fs::symlink_metadata(state.path().join("record"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(state.list(10).unwrap(), ["record"]);
    fs::remove_file(state.path().join("record")).unwrap();
}

#[test]
fn target_junction_swap_mid_write_is_refused() {
    let (_guard, root) = tmp();
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("victim"), b"intact").unwrap();
    let state = StateDir::create(&root.join("state")).unwrap();
    state.write("record", b"old").unwrap();
    let (temp, mut file) = state.create_temp("record").unwrap();
    file.write_all(b"before-").unwrap();
    fs::remove_file(state.path().join("record")).unwrap();
    if let Err(e) = junction(&state.path().join("record"), &outside) {
        drop(file);
        state.dir.unlink(&temp).unwrap();
        eprintln!("SKIP target junction case: {e}");
        return;
    }
    file.write_all(b"after").unwrap();
    drop(file);
    assert!(matches!(
        state.dir.rename_replace(&temp, "record"),
        Err(FsError::Symlink(_))
    ));
    state.dir.unlink(&temp).unwrap();
    assert!(state.write("record", b"attack").is_err());
    assert!(state.write_new("record", b"attack").is_err());
    assert!(state.read("record", 100).is_err());
    assert!(state.remove("record").is_err());
    assert!(matches!(state.child("record"), Err(FsError::Symlink(_))));
    assert!(matches!(
        StateDir::open(&state.path().join("record")),
        Err(FsError::Symlink(_))
    ));
    assert!(matches!(
        StateDir::create(&state.path().join("record/new")),
        Err(FsError::Symlink(_))
    ));
    assert_eq!(fs::read(outside.join("victim")).unwrap(), b"intact");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    assert_eq!(state.list(10).unwrap(), ["record"]);
    fs::remove_dir(state.path().join("record")).unwrap();
}

#[test]
fn windows_names_and_no_replace_publication() {
    let (_guard, root) = tmp();
    let state = StateDir::create(&root.join("state")).unwrap();
    for name in [
        "record:stream",
        "record.",
        "record ",
        "x*y",
        "x?y",
        "x\u{1f}y",
    ] {
        assert!(matches!(
            state.write(name, b"refused"),
            Err(FsError::BadName(_))
        ));
    }
    let name = "x".repeat(255); // Internal temp suffix must not shrink public name cap.
    state.write_new(&name, b"first").unwrap();
    assert!(matches!(
        state.write_new(&name, b"second"),
        Err(FsError::AlreadyExists(_))
    ));
    assert_eq!(state.read(&name, 100).unwrap(), b"first");
    state.write(&name, b"replace").unwrap();
    assert_eq!(state.read(&name, 100).unwrap(), b"replace");
    assert!(matches!(state.list(0), Err(FsError::TooManyEntries(0))));
    assert_eq!(state.list(1).unwrap(), [name]);
}

#[test]
fn windows_name_limit_counts_utf16_units_and_keeps_temps_sweepable() {
    let (_guard, root) = tmp();
    let state = StateDir::create(&root.join("state")).unwrap();
    for name in [
        "\u{e9}".repeat(255),
        format!("{}x", "\u{1f600}".repeat(127)),
    ] {
        assert_eq!(name.encode_utf16().count(), 255);
        state.write_new(&name, b"first").unwrap();
        assert_eq!(state.read(&name, 100).unwrap(), b"first");
        state.write(&name, b"replace").unwrap();
        assert_eq!(state.read(&name, 100).unwrap(), b"replace");
        assert_eq!(state.list(1).unwrap(), std::slice::from_ref(&name));
        assert!(state.remove(&name).unwrap());
        let (temp, file) = state.create_temp(&name).unwrap();
        assert!(temp.encode_utf16().count() <= 255);
        drop(file);
        // This process still owns the generated temp; age cannot reclaim it.
        assert_eq!(state.sweep_temps(std::time::Duration::ZERO).unwrap(), 0);
        assert!(state.exists(&temp).unwrap());
        assert!(state.remove(&temp).unwrap());
        assert!(state.list(0).unwrap().is_empty());
    }
    for name in [
        "x".repeat(256),
        "\u{e9}".repeat(256),
        "\u{1f600}".repeat(128),
    ] {
        assert!(matches!(
            state.write(&name, b"refused"),
            Err(FsError::BadName(_))
        ));
        assert!(matches!(
            state.write_new(&name, b"refused"),
            Err(FsError::BadName(_))
        ));
        assert!(matches!(state.child(&name), Err(FsError::BadName(_))));
    }
}

#[test]
fn rename_while_open_is_refused_by_default_and_allowed_by_the_option() {
    let (_guard, root) = tmp();
    let options = StateOptions {
        allow_rename_while_open: true,
        ..StateOptions::default()
    };
    let path = root.join("state");
    let state = StateDir::create_with(&path, &options).unwrap();
    state.write("before", b"1").unwrap();
    let moved = root.join("moved");
    fs::rename(&path, &moved).expect("FILE_SHARE_DELETE lets the directory be renamed");
    fs::create_dir(&path).unwrap();
    state.write("after", b"2").unwrap();
    assert_eq!(fs::read(moved.join("after")).unwrap(), b"2");
    assert_eq!(state.read("before", 10).unwrap(), b"1");
    assert!(fs::read_dir(&path).unwrap().next().is_none());
    let child = state.child("kid").unwrap();
    child.write("n", b"3").unwrap();
    assert_eq!(fs::read(moved.join("kid/n")).unwrap(), b"3");
    assert!(state.remove("before").unwrap());
    assert!(!moved.join("before").exists());
    // The default still pins the path.
    drop((child, state));
    let pinned = StateDir::create(&moved).unwrap();
    assert!(fs::rename(&moved, root.join("again")).is_err());
    drop(pinned);
}

#[test]
fn list_entries_reports_kinds_from_the_directory_records() {
    use crate::EntryKind;
    let (_guard, root) = tmp();
    let state = StateDir::create(&root.join("state")).unwrap();
    state.write("file", b"x").unwrap();
    state.child("sub").unwrap();
    let target = root.join("outside");
    fs::write(&target, b"o").unwrap();
    let linked = symlink_file(&target, root.join("state/link")).is_ok();
    let junctioned = {
        let outside = root.join("outside-dir");
        fs::create_dir(&outside).unwrap();
        junction(&root.join("state/junction"), &outside).is_ok()
    };
    let entries = state.list_entries(100).unwrap();
    let kind = |name: &str| entries.iter().find(|e| e.name == name).unwrap().kind;
    assert_eq!(kind("file"), EntryKind::File);
    assert_eq!(kind("sub"), EntryKind::Dir);
    if linked {
        assert_eq!(kind("link"), EntryKind::Symlink);
    }
    if junctioned {
        assert_eq!(kind("junction"), EntryKind::Symlink);
    }
    // One long name over the unix write cap and a lone surrogate-free Unicode name both list.
    state.write(&"é".repeat(120), b"u").unwrap();
    assert!(state.list(100).unwrap().contains(&"é".repeat(120)));
}

#[test]
fn directory_removal_and_creation_need_no_directory_flush() {
    let (_guard, root) = tmp();
    let state = StateDir::create(&root.join("state")).unwrap();
    // `sync` is the documented no-op; every mutation that claims durability still succeeds.
    assert!(state.dir.sync(SyncPolicy::Strict).unwrap().is_none());
    state.write("a", b"1").unwrap();
    state.write_new("b", b"2").unwrap();
    state.child("c").unwrap();
    assert!(state.remove("a").unwrap());
    assert_eq!(state.write_reporting("d", b"3").unwrap(), SyncLevel::Full);
}

#[test]
fn inherit_acl_option_leaves_the_parent_acl_in_charge() {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetNamedSecurityInfoW(
            path: *const u16,
            object_type: u32,
            information: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor: *mut c_void,
            revision: u32,
            information: u32,
            out: *mut *mut u16,
            len: *mut u32,
        ) -> i32;
    }
    fn sddl(path: &Path) -> String {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let mut descriptor = null_mut();
            let status = GetNamedSecurityInfoW(
                wide.as_ptr(),
                1,
                4,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            );
            assert_eq!(status, 0, "GetNamedSecurityInfoW({})", path.display());
            let mut text = null_mut();
            assert_ne!(
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    1,
                    4,
                    &mut text,
                    null_mut()
                ),
                0
            );
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let out = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            LocalFree(descriptor);
            out
        }
    }
    let (_guard, root) = tmp();
    let owner_only = root.join("owner/state");
    let inherit = root.join("inherit/state");
    let options = StateOptions {
        privacy: crate::Privacy::OwnerOnlyUnixInheritWindows,
        ..StateOptions::default()
    };
    let strict = StateDir::create(&owner_only).unwrap();
    let loose = StateDir::create_with(&inherit, &options).unwrap();
    strict.write("f", b"x").unwrap();
    loose.write("f", b"x").unwrap();
    for (label, path) in [
        (
            "owner-only ancestor",
            owner_only.parent().unwrap().to_path_buf(),
        ),
        ("owner-only dir", owner_only.clone()),
        ("owner-only file", owner_only.join("f")),
    ] {
        let text = sddl(&path);
        println!("{label}: {text}");
        assert!(text.starts_with("D:P"), "{label} must be protected: {text}");
    }
    for (label, path) in [
        ("inherit ancestor", inherit.parent().unwrap().to_path_buf()),
        ("inherit dir", inherit.clone()),
        ("inherit file", inherit.join("f")),
    ] {
        let text = sddl(&path);
        println!("{label}: {text}");
        assert!(
            !text.starts_with("D:P") && text.contains("ID;"),
            "{label} must carry the parent's inherited ACL: {text}"
        );
    }
}
