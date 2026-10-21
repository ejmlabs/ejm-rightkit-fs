//! Windows: what DACL a private atomic write leaves on the destination, for each `ReplaceMode`,
//! measured by reading the DACL back (`GetNamedSecurityInfoW`, rendered as SDDL).
//!
//! The reference is the DACL the crate gives a fresh private file (`restrict_owner_only`: one
//! protected, non-inherited ACE for the current user). Every private write must end with that same
//! SDDL, whether the destination was missing, already private, or broad (a different DACL from the
//! temp's). Every mode publishes by moving the temp file, which carries its own DACL, so the
//! broad-destination case must still end owner-only.
//!
//! The observed SDDL of each case is printed (run with `--nocapture`) so a qualification run records
//! what the Windows build did.

#![cfg(windows)]

use std::ffi::c_void;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use rightkit_fs::{atomic_write_with_options, AtomicWriteOptions, Privacy, ReplaceMode};

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
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}

const SE_FILE_OBJECT: u32 = 1;
const DACL_SECURITY_INFORMATION: u32 = 0x4;
const SDDL_REVISION_1: u32 = 1;

/// The DACL of `path` as an SDDL string, e.g. `D:P(A;;FA;;;S-1-5-21-...)` (see below for `AI`).
fn dacl_sddl(path: &Path) -> String {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let mut descriptor = std::ptr::null_mut();
        let status = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        );
        assert_eq!(
            status,
            0,
            "GetNamedSecurityInfoW({}) failed: {status}",
            path.display()
        );
        let mut text = std::ptr::null_mut();
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            std::ptr::null_mut(),
        );
        assert_ne!(ok, 0, "SDDL conversion failed");
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        let sddl = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        LocalFree(descriptor);
        // A private temp is created with `D:P(...)` and a destination narrowed afterwards by
        // `SetNamedSecurityInfoW` shows `D:PAI(...)`: the `AI` flag is inheritance bookkeeping, not
        // a different access control list, so both compare as `D:P(...)`.
        sddl.replacen("D:PAI(", "D:P(", 1)
    }
}

/// Number of ACEs in an SDDL DACL string (each is one parenthesised group).
fn ace_count(sddl: &str) -> usize {
    sddl.matches('(').count()
}

/// Principals that would make a file broader than private.
fn names_broad_principal(sddl: &str) -> bool {
    // Aliases (World, Authenticated Users, Users, Interactive, Everyone's SID, BUILTIN\Users).
    [
        ";;;WD)",
        ";;;AU)",
        ";;;BU)",
        ";;;IU)",
        ";;;S-1-1-0)",
        ";;;S-1-5-32-545)",
        ";;;S-1-5-11)",
    ]
    .iter()
    .any(|p| sddl.contains(p))
}

fn windows_build() -> String {
    std::process::Command::new("cmd")
        .args(["/c", "ver"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|e| format!("unknown ({e})"))
}

fn private_write(path: &Path, mode: ReplaceMode, bytes: &'static [u8]) {
    let options = AtomicWriteOptions {
        private: true,
        replace: mode,
        ..AtomicWriteOptions::default()
    };
    atomic_write_with_options(path, options, |f| f.write_all(bytes)).unwrap();
}

/// A destination broader than private: the inherited DACL plus an explicit Everyone read ACE.
fn make_broad(path: &Path) {
    std::fs::write(path, b"old-broad").unwrap();
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/grant", "*S-1-1-0:(R)"])
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "icacls could not widen the destination");
}

fn reference_sddl(dir: &Path) -> String {
    let reference = dir.join("reference.bin");
    rightkit_fs::private::write_private_file(&reference, b"ref").unwrap();
    let sddl = dacl_sddl(&reference);
    assert!(
        sddl.starts_with("D:P"),
        "the private reference must be protected: {sddl}"
    );
    assert_eq!(
        ace_count(&sddl),
        1,
        "the private reference is one ACE: {sddl}"
    );
    assert!(!names_broad_principal(&sddl), "{sddl}");
    sddl
}

fn check(mode: ReplaceMode) {
    let dir = tempfile::tempdir().unwrap();
    let reference = reference_sddl(dir.path());
    println!("build: {}", windows_build());
    println!("{mode:?} reference (fresh private file): {reference}");

    // 1. Destination missing.
    let missing = dir.path().join("missing.bin");
    private_write(&missing, mode, b"new");
    let got = dacl_sddl(&missing);
    println!("{mode:?} destination missing -> {got}");
    assert_eq!(
        got, reference,
        "{mode:?}: new private file is not owner-only"
    );

    // 2. Destination already private.
    private_write(&missing, mode, b"newer");
    let got = dacl_sddl(&missing);
    println!("{mode:?} destination private -> {got}");
    assert_eq!(
        got, reference,
        "{mode:?}: private over private changed the DACL"
    );
    assert_eq!(std::fs::read(&missing).unwrap(), b"newer");

    // 3. Destination broad: its DACL differs from the temp's.
    let broad = dir.path().join("broad.bin");
    make_broad(&broad);
    let before = dacl_sddl(&broad);
    println!("{mode:?} destination broad (before) -> {before}");
    assert_ne!(
        before, reference,
        "precondition: the destination must start broader"
    );
    assert!(names_broad_principal(&before), "precondition: {before}");
    private_write(&broad, mode, b"now-private");
    let got = dacl_sddl(&broad);
    println!("{mode:?} destination broad (after private write) -> {got}");
    assert_eq!(std::fs::read(&broad).unwrap(), b"now-private");
    assert!(
        !names_broad_principal(&got) && got == reference,
        "{mode:?}: a private write left the destination broader than private: {got}"
    );
}

#[test]
fn rename_mode_leaves_private_writes_owner_only() {
    check(ReplaceMode::Rename);
}

#[test]
fn move_write_through_mode_leaves_private_writes_owner_only() {
    check(ReplaceMode::MoveWriteThrough);
}

/// Informational: a NON-private write over a private destination. Not asserted; it records which
/// DACL each mode keeps (a rename and a move both take the temp's).
#[test]
fn record_plain_write_over_private_destination() {
    for mode in [ReplaceMode::Rename, ReplaceMode::MoveWriteThrough] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kept.bin");
        private_write(&path, mode, b"private");
        let options = AtomicWriteOptions {
            replace: mode,
            ..AtomicWriteOptions::default()
        };
        atomic_write_with_options(&path, options, |f| f.write_all(b"plain")).unwrap();
        println!(
            "{mode:?} plain write over private destination -> {}",
            dacl_sddl(&path)
        );
    }
}

/// `Privacy::OwnerOnlyUnixInheritWindows` takes no ACL step on Windows (the file carries the parent's
/// inherited ACL, never the protected owner-only one); `Privacy::OwnerOnly` equals `private: true`.
#[test]
fn privacy_variants_on_windows() {
    let dir = tempfile::tempdir().unwrap();
    let reference = reference_sddl(dir.path());
    println!("build: {}", windows_build());
    for mode in [ReplaceMode::Rename, ReplaceMode::MoveWriteThrough] {
        let write = |name: &str, privacy: Privacy, over_private: bool| {
            let path = dir.path().join(name);
            if over_private {
                private_write(&path, mode, b"seed");
            }
            let options = AtomicWriteOptions {
                privacy,
                replace: mode,
                ..AtomicWriteOptions::default()
            };
            atomic_write_with_options(&path, options, |f| f.write_all(b"x")).unwrap();
            dacl_sddl(&path)
        };
        let owner = write(&format!("owner-{mode:?}.bin"), Privacy::OwnerOnly, false);
        assert_eq!(owner, reference, "{mode:?}: OwnerOnly");
        let inherit = write(
            &format!("inherit-{mode:?}.bin"),
            Privacy::OwnerOnlyUnixInheritWindows,
            false,
        );
        println!("{mode:?} OwnerOnlyUnixInheritWindows (new file) -> {inherit}");
        assert!(
            !inherit.starts_with("D:P"),
            "{mode:?}: the inherited ACL is not protected: {inherit}"
        );
        assert_ne!(inherit, reference, "{mode:?}: no ACL step ran");
        let plain = write(&format!("plain-{mode:?}.bin"), Privacy::Inherit, false);
        assert_eq!(
            plain, inherit,
            "{mode:?}: Inherit and OwnerOnlyUnixInheritWindows agree on Windows"
        );
    }
}

/// The owner-only probe must inspect owner and DACL on Windows; it cannot be the former
/// unconditional success stub. A broad DACL is false, a missing object is an I/O error, and a
/// native private creation verifies both before returning a writable handle.
#[test]
fn owner_only_probe_reads_real_windows_security_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private.bin");
    rightkit_fs::private::write_private_file(&private, b"secret").unwrap();
    assert!(
        rightkit_fs::private::is_owner_only(&private).unwrap(),
        "fresh private file must have current TokenUser owner and protected owner-only DACL"
    );

    let broad = dir.path().join("broad.bin");
    make_broad(&broad);
    assert!(
        !rightkit_fs::private::is_owner_only(&broad).unwrap(),
        "an explicit Everyone read ACE is not owner-only"
    );

    let missing = dir.path().join("missing.bin");
    assert!(
        rightkit_fs::private::is_owner_only(&missing).is_err(),
        "a missing path is not proof of owner-only state"
    );
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSidToSidW(string_sid: *const u16, sid: *mut *mut c_void) -> i32;
    fn SetNamedSecurityInfoW(
        path: *const u16,
        object_type: u32,
        information: u32,
        owner: *mut c_void,
        group: *mut c_void,
        dacl: *mut c_void,
        sacl: *mut c_void,
    ) -> u32;
}

/// An elevated process creates files owned by BUILTIN\Administrators, not by its user account.
/// Repair (`restrict_owner_only`) and the probe must accept that owner, because the Administrators
/// group is enabled in the elevated token. The old rule demanded the user SID and failed with
/// `NotOwned`. Needs an elevated token to hand the file to Administrators; otherwise the case is
/// covered by the pure `owner_is_trusted` unit tests and this test returns early.
#[test]
fn administrators_owned_object_is_repaired_when_elevated() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("admin-owned.bin");
    std::fs::write(&file, b"x").unwrap();
    let wide: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
    let sid_text: Vec<u16> = "S-1-5-32-544\0".encode_utf16().collect();
    let mut admins = std::ptr::null_mut();
    // SAFETY: NUL-terminated SID string; the returned SID is freed below.
    assert_ne!(
        unsafe { ConvertStringSidToSidW(sid_text.as_ptr(), &mut admins) },
        0
    );
    const OWNER_SECURITY_INFORMATION: u32 = 0x1;
    // SAFETY: NUL-terminated path and a valid SID.
    let set = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            admins,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    // SAFETY: allocated by ConvertStringSidToSidW.
    unsafe { LocalFree(admins) };
    if set != 0 {
        eprintln!(
            "not elevated (SetNamedSecurityInfoW owner = {set}); Administrators-owner case skipped"
        );
        return;
    }
    rightkit_fs::private::restrict_owner_only(&file).expect(
        "Administrators-owned object must be repairable when the token holds Administrators",
    );
    assert!(rightkit_fs::private::is_owner_only(&file).unwrap());
    // The repair rewrites the owner to the token user, so the object stays usable unelevated
    // (where BUILTIN\Administrators is deny-only, not the owner).
    let owner = owner_sddl(&file);
    assert!(
        !owner.contains("S-1-5-32-544"),
        "owner must be the token user after repair: {owner}"
    );
}

/// The owner of `path` as SDDL (`O:S-1-...`).
fn owner_sddl(path: &Path) -> String {
    const OWNER_SECURITY_INFORMATION: u32 = 0x1;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let mut descriptor = std::ptr::null_mut();
        let status = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        );
        assert_eq!(status, 0, "GetNamedSecurityInfoW owner failed: {status}");
        let mut text = std::ptr::null_mut();
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            OWNER_SECURITY_INFORMATION,
            &mut text,
            std::ptr::null_mut(),
        );
        assert_ne!(ok, 0, "owner SDDL conversion failed");
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        let sddl = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        LocalFree(descriptor);
        sddl
    }
}

/// Hardening a file that another handle holds open without `FILE_SHARE_DELETE` (what SQLite does
/// to a live database) must succeed: the repair handle asks only for DACL access and shares
/// read, write and delete, so neither side hits a sharing violation.
#[test]
fn restrict_owner_only_succeeds_while_another_handle_holds_the_file_without_delete_share() {
    use std::os::windows::fs::OpenOptionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.sqlite");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(0x3) // FILE_SHARE_READ | FILE_SHARE_WRITE, no FILE_SHARE_DELETE
        .open(&path)
        .unwrap();
    rightkit_fs::private::restrict_owner_only(&path)
        .expect("hardening must not hit a sharing violation on a held file");
    assert!(rightkit_fs::private::is_owner_only(&path).unwrap());
    drop(held);
}
