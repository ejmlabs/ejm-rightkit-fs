//! `Privacy`: owner-only mode bits on Unix for `OwnerOnly` and `OwnerOnlyUnixInheritWindows`, the
//! previous behaviour for `private: bool`, and no change for `Inherit`. The Windows ACLs for each
//! variant are read back in `atomic_dacl_windows.rs`.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use rightkit_fs::{atomic_write_with_options, AtomicWriteOptions, Privacy};

fn mode_after(options: AtomicWriteOptions) -> u32 {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    atomic_write_with_options(&path, options, |f| f.write_all(b"x")).unwrap();
    std::fs::metadata(&path).unwrap().permissions().mode() & 0o777
}

#[test]
fn private_true_and_the_owner_only_variants_are_0600_on_unix() {
    let base = AtomicWriteOptions::default();
    assert_eq!(
        mode_after(AtomicWriteOptions {
            private: true,
            ..base
        }),
        0o600
    );
    assert_eq!(
        mode_after(AtomicWriteOptions {
            privacy: Privacy::OwnerOnly,
            ..base
        }),
        0o600
    );
    assert_eq!(
        mode_after(AtomicWriteOptions {
            privacy: Privacy::OwnerOnlyUnixInheritWindows,
            ..base
        }),
        0o600
    );
}

#[test]
fn inherit_is_unchanged_and_private_true_wins_over_privacy() {
    let base = AtomicWriteOptions::default();
    let inherit = mode_after(AtomicWriteOptions {
        privacy: Privacy::Inherit,
        ..base
    });
    assert_ne!(
        inherit, 0o600,
        "the default write is not forced to 0600 (umask applies)"
    );
    assert_eq!(inherit, mode_after(base));
    assert_eq!(
        mode_after(AtomicWriteOptions {
            private: true,
            privacy: Privacy::Inherit,
            ..base
        }),
        0o600
    );
    assert_eq!(base.effective_privacy(), Privacy::Inherit);
    assert_eq!(
        AtomicWriteOptions {
            private: true,
            ..base
        }
        .effective_privacy(),
        Privacy::OwnerOnly
    );
}

#[test]
fn parents_created_for_an_owner_only_write_are_0700_on_unix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a").join("b").join("f.bin");
    let options = AtomicWriteOptions {
        create_parents: true,
        privacy: Privacy::OwnerOnlyUnixInheritWindows,
        ..AtomicWriteOptions::default()
    };
    atomic_write_with_options(&path, options, |f| f.write_all(b"x")).unwrap();
    let parent = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(parent, 0o700);
}

/// A symlink at the destination lends nothing: its target's mode is not copied to the new file
/// (the existing-mode rule applies to a regular file only), the link itself is replaced, and the
/// target is untouched.
#[test]
fn a_symlink_at_the_destination_is_replaced_not_followed_and_lends_no_mode() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("target.bin");
    std::fs::write(&secret, b"target content").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.path().join("link.bin");
    std::os::unix::fs::symlink(&secret, &link).unwrap();

    let options = AtomicWriteOptions::default();
    atomic_write_with_options(&link, options, |f| f.write_all(b"new")).unwrap();

    let meta = std::fs::symlink_metadata(&link).unwrap();
    assert!(meta.is_file(), "the link was replaced by a regular file");
    assert_eq!(std::fs::read(&link).unwrap(), b"new");
    assert_eq!(std::fs::read(&secret).unwrap(), b"target content");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        mode_after(options),
        "the mode is the one a fresh file gets, not the link target's 0600"
    );
}
