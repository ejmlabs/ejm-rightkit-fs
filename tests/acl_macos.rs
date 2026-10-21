//! Owner-only means owner-only under macOS extended ACLs. The mode bits `0600`/`0700` do not
//! constrain an ACL: macOS evaluates ACL `allow` entries first, and a new file or directory
//! inherits the inheritable entries of its parent's ACL. A parent carrying
//! `everyone allow read,file_inherit,directory_inherit` would make every `0600` file created
//! below it readable by everyone.
//!
//! One predicate decides everything: an ACL is unsafe iff it has an `allow` entry; a `deny` entry
//! cannot widen access, so deny-only ACLs pass. The crate reads the parent's ACL before it creates
//! anything private and refuses (`FsError::UnsafeAcl`) when an inheritable `allow` entry would be
//! handed on. A temporary entry that carries an `allow` entry anyway is removed while still empty
//! and refused; a visible permanent empty-file name is retained and refused. No ACL is ever
//! removed from an inode (that cannot revoke a descriptor already open),
//! and `restrict_owner_only` refuses an entry that has an `allow` entry instead of repairing it.
//! These tests make the ACLs with `/bin/chmod +a`, read them back with `/bin/ls -lde`, and need a
//! macOS volume that holds ACLs (APFS does). They have not been run in the tree that wrote them.

#![cfg(target_os = "macos")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use rightkit_fs::{
    atomic_write, atomic_write_with_options, create_private_new, ensure_private_dir,
    restrict_owner_only, AtomicWriteOptions, FsError, Privacy, StateDir,
};

fn is_owner_only_for_test(path: &Path) -> bool {
    rightkit_fs::private::is_owner_only(path).unwrap()
}

const INHERITING_ALLOW: &str = "everyone allow read,file_inherit,directory_inherit";

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

fn add_acl(path: &Path, entry: &str) {
    let status = Command::new("/bin/chmod")
        .arg("+a")
        .arg(entry)
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "chmod +a {entry:?} {}", path.display());
}

/// Whether any of `path`'s ACL entries is an `allow` entry.
fn has_allow_entry(path: &Path) -> bool {
    acl_entries(path)
        .iter()
        .any(|line| line.contains(" allow "))
}

/// The ACL entry lines `ls -lde` prints under the file line (` 0: group:everyone allow read`).
fn acl_entries(path: &Path) -> Vec<String> {
    let output = Command::new("/bin/ls")
        .arg("-lde")
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .skip(1)
        .filter(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_string)
        .collect()
}

fn parent_with(entry: &str) -> (tempfile::TempDir, PathBuf) {
    let (guard, root) = tmp();
    let parent = root.join("p");
    std::fs::create_dir(&parent).unwrap();
    add_acl(&parent, entry);
    assert!(
        !acl_entries(&parent).is_empty(),
        "the volume holds ACLs (precondition)"
    );
    (guard, parent)
}

fn names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

fn private_options() -> [AtomicWriteOptions; 3] {
    let base = AtomicWriteOptions::default();
    [
        AtomicWriteOptions {
            private: true,
            ..base
        },
        AtomicWriteOptions {
            privacy: Privacy::OwnerOnly,
            ..base
        },
        AtomicWriteOptions {
            privacy: Privacy::OwnerOnlyUnixInheritWindows,
            ..base
        },
    ]
}

#[test]
fn an_inheriting_parent_refuses_a_private_atomic_write_and_creates_nothing() {
    let (_g, parent) = parent_with(INHERITING_ALLOW);
    for options in private_options() {
        let error =
            atomic_write_with_options(&parent.join("secret"), options, |f| f.write_all(b"secret"))
                .expect_err("the owner-only guarantee cannot be established");
        assert!(matches!(error, FsError::UnsafeAcl(_)), "{error:?}");
    }
    assert!(
        names(&parent).is_empty(),
        "no temp, no file: {:?}",
        names(&parent)
    );
    // A write that did not ask for privacy is the caller's business and is not refused.
    atomic_write(&parent.join("plain"), b"x").unwrap();
}

#[test]
fn an_inheriting_parent_refuses_private_parents() {
    let (_g, parent) = parent_with(INHERITING_ALLOW);
    let options = AtomicWriteOptions {
        create_parents: true,
        privacy: Privacy::OwnerOnly,
        ..AtomicWriteOptions::default()
    };
    let error =
        atomic_write_with_options(&parent.join("new/deep/f"), options, |f| f.write_all(b"x"))
            .expect_err("a private directory cannot be made under this parent");
    assert!(matches!(error, FsError::UnsafeAcl(_)), "{error:?}");
    assert!(!parent.join("new").exists());
    let error = ensure_private_dir(&parent.join("dir")).expect_err("ensure_private_dir");
    assert!(matches!(error, FsError::UnsafeAcl(_)), "{error:?}");
    assert!(!parent.join("dir").exists());
}

#[test]
fn create_private_new_refuses_an_inheriting_parent_and_creates_nothing() {
    let (_g, parent) = parent_with(INHERITING_ALLOW);
    let error = create_private_new(&parent.join("log")).expect_err("refused");
    assert!(matches!(error, FsError::UnsafeAcl(_)), "{error:?}");
    assert!(names(&parent).is_empty());
}

#[test]
fn state_dir_refuses_an_inheriting_parent() {
    let (_g, parent) = parent_with(INHERITING_ALLOW);
    let error = StateDir::create(&parent.join("s"))
        .map(|_| ())
        .expect_err("mkdir refused");
    assert!(matches!(error, FsError::UnsafeAcl(_)), "{error:?}");
    assert!(!parent.join("s").exists());

    // A state directory that gets the ACL later refuses to make children and temp files in it.
    let clean = parent.parent().unwrap().join("clean");
    let state = StateDir::create(&clean).unwrap();
    add_acl(&clean, INHERITING_ALLOW);
    let error = state.child("kid").map(|_| ()).expect_err("child refused");
    assert!(matches!(error, FsError::UnsafeAcl(_)), "{error:?}");
    let error = state.write("record", b"secret").expect_err("temp refused");
    assert!(matches!(error, FsError::UnsafeAcl(_)), "{error:?}");
    let error = state
        .create_new_empty("lock")
        .expect_err("direct creation refused");
    assert!(matches!(error, FsError::UnsafeAcl(_)), "{error:?}");
    assert!(
        names(&clean).is_empty(),
        "nothing was created: {:?}",
        names(&clean)
    );
}

#[cfg(feature = "testing")]
#[test]
fn direct_empty_acl_failure_keeps_the_inode_and_blocks_private_reopen() {
    use rightkit_fs::handle::ReadOptions;
    use rightkit_fs::{state::testing::on_empty_created, FileKey};
    use std::sync::{Arc, Mutex};

    let (_guard, root) = tmp();
    let state = StateDir::create(&root.join("state")).unwrap();
    let path = state.path().join("lock");
    let changed = path.clone();
    let key = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&key);
    let hook = on_empty_created(move |file| {
        *captured.lock().unwrap() = Some(FileKey::of_file(file, Path::new("lock")).unwrap());
        add_acl(&changed, "everyone allow read");
        assert!(
            has_allow_entry(&changed),
            "the actual ACL mutation is a precondition"
        );
        Ok(())
    });
    assert!(matches!(
        state.create_new_empty("lock"),
        Err(FsError::UnsafeAcl(_))
    ));
    drop(hook);
    let strict = ReadOptions::new()
        .require_single_link(true)
        .require_owner_only(true);
    assert!(matches!(
        state.open_read("lock", &strict),
        Err(FsError::UnsafeAcl(_))
    ));
    let plain = state.open_read("lock", &ReadOptions::new()).unwrap();
    assert_eq!(
        Some(FileKey::of_file(&plain, Path::new("lock")).unwrap()),
        *key.lock().unwrap()
    );
    assert!(has_allow_entry(&path), "failure must not repair the ACL");
    assert!(matches!(
        state.create_new_empty("lock"),
        Err(FsError::AlreadyExists(_))
    ));
    assert_eq!(names(state.path()), vec!["lock"]);
}

#[cfg(feature = "lock")]
#[test]
fn a_lock_file_is_refused_under_an_inheriting_parent() {
    let (_g, parent) = parent_with(INHERITING_ALLOW);
    let error = rightkit_fs::FileLock::try_acquire(&parent.join("x.lock"))
        .map(|_| ())
        .expect_err("refused");
    assert!(matches!(error, FsError::UnsafeAcl(_)), "{error:?}");
    assert!(!parent.join("x.lock").exists());
}

/// An entry that is not inheritable cannot reach a new entry: the write is accepted and the new
/// file ends without an ACL.
#[test]
fn a_non_inheritable_entry_is_accepted_and_the_new_file_has_no_acl() {
    let entry = "everyone allow read";
    let (_g, parent) = parent_with(entry);
    let target = parent.join("secret");
    atomic_write_with_options(&target, private_options()[0], |f| f.write_all(b"secret"))
        .unwrap_or_else(|e| panic!("{entry}: {e:?}"));
    assert_eq!(std::fs::read(&target).unwrap(), b"secret");
    assert!(
        acl_entries(&target).is_empty(),
        "{entry}: {:?}",
        acl_entries(&target)
    );
    let file = create_private_new(&parent.join("log")).unwrap();
    drop(file);
    assert!(acl_entries(&parent.join("log")).is_empty(), "{entry}");
}

/// A deny entry cannot widen access, so a parent whose only inheritable entry is a deny entry is
/// accepted (as 0.3.3 accepted it): every private creation works, nothing is refused, and the new
/// entries carry no `allow` entry. (The deny entry used is harmless to this crate's own writes.)
#[test]
fn an_inheritable_deny_entry_is_accepted() {
    use std::os::unix::fs::PermissionsExt;
    let (_g, parent) = parent_with("everyone deny writeextattr,file_inherit,directory_inherit");
    for options in private_options() {
        let target = parent.join(format!("secret-{:?}", options.privacy));
        atomic_write_with_options(&target, options, |f| f.write_all(b"secret"))
            .unwrap_or_else(|e| panic!("{options:?}: {e:?}"));
        assert_eq!(std::fs::read(&target).unwrap(), b"secret");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!has_allow_entry(&target), "{:?}", acl_entries(&target));
    }
    drop(create_private_new(&parent.join("log")).expect("create_private_new accepts deny-only"));
    assert!(!has_allow_entry(&parent.join("log")));
    ensure_private_dir(&parent.join("dir")).expect("ensure_private_dir accepts deny-only");
    assert!(!has_allow_entry(&parent.join("dir")));
}

/// An inheritable `allow` entry is refused (the guard), whether it is inherited by files only,
/// directories only or both, and nothing is created.
#[test]
fn an_inheritable_allow_entry_is_refused_and_creates_nothing() {
    for entry in [
        "everyone allow read,file_inherit",
        "everyone allow read,directory_inherit",
        INHERITING_ALLOW,
    ] {
        let (_g, parent) = parent_with(entry);
        let error = atomic_write_with_options(&parent.join("secret"), private_options()[0], |f| {
            f.write_all(b"secret")
        })
        .expect_err(entry);
        assert!(matches!(error, FsError::UnsafeAcl(_)), "{entry}: {error:?}");
        let error = create_private_new(&parent.join("log")).expect_err(entry);
        assert!(matches!(error, FsError::UnsafeAcl(_)), "{entry}: {error:?}");
        assert!(
            names(&parent).is_empty(),
            "{entry}: nothing was created: {:?}",
            names(&parent)
        );
    }
}

/// Verification and refusal of an existing entry: a `0600` file with an ACL `allow` entry is not
/// owner-only, and `restrict_owner_only` does not repair it. It narrows the mode, leaves the ACL
/// where it is (removing it could not revoke a descriptor somebody already holds) and refuses.
#[test]
fn restrict_owner_only_refuses_an_allow_entry_and_leaves_the_acl_alone() {
    use std::os::unix::fs::PermissionsExt;
    let (_g, root) = tmp();
    let file = root.join("f");
    std::fs::write(&file, b"x").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    add_acl(&file, "everyone allow read");
    assert!(!is_owner_only_for_test(&file), "readable through the mode");
    let error = restrict_owner_only(&file).expect_err("an allow entry is refused, not removed");
    assert!(matches!(error, FsError::UnsafeAcl(_)), "{error:?}");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
        0o600,
        "the mode was narrowed"
    );
    assert!(
        has_allow_entry(&file),
        "the ACL is still there: it is not repaired"
    );
    assert!(
        !is_owner_only_for_test(&file),
        "mode 0600 but readable through the ACL"
    );

    let dir = root.join("d");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    add_acl(&dir, "everyone allow list,search");
    assert!(!is_owner_only_for_test(&dir));
    let error = restrict_owner_only(&dir).expect_err("refused");
    assert!(matches!(error, FsError::UnsafeAcl(_)), "{error:?}");
    assert!(has_allow_entry(&dir));
}

/// A deny-only ACL passes `restrict_owner_only` and `is_owner_only`, and is left where it is.
#[test]
fn restrict_owner_only_accepts_a_deny_only_acl() {
    use std::os::unix::fs::PermissionsExt;
    let (_g, root) = tmp();
    let file = root.join("f");
    std::fs::write(&file, b"x").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    add_acl(&file, "everyone deny writeextattr");
    assert!(!is_owner_only_for_test(&file), "the mode is still 0644");
    restrict_owner_only(&file).expect("a deny-only ACL does not widen access");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    assert!(is_owner_only_for_test(&file), "0600 and no allow entry");
    assert!(
        !acl_entries(&file).is_empty(),
        "the deny entry is left in place"
    );

    let dir = root.join("d");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    add_acl(&dir, "everyone deny writeextattr");
    restrict_owner_only(&dir).expect("a deny-only ACL does not widen access");
    assert!(is_owner_only_for_test(&dir));
}
