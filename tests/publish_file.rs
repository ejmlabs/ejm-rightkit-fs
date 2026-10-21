//! `publish_file`: durably publish a file another process already wrote.

use rightkit_fs::{publish_file, publish_file_with, AtomicWriteOptions, FsError, ReplaceMode};

fn entries(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn publishes_a_new_file_and_removes_the_source_name() {
    let dir = tempfile::tempdir().unwrap();
    let (from, to) = (dir.path().join("out.part"), dir.path().join("out.mp4"));
    std::fs::write(&from, b"encoded").unwrap();
    publish_file(&from, &to).unwrap();
    assert_eq!(std::fs::read(&to).unwrap(), b"encoded");
    assert_eq!(entries(dir.path()), vec!["out.mp4".to_string()]);
}

#[test]
fn replaces_an_existing_destination() {
    let dir = tempfile::tempdir().unwrap();
    let (from, to) = (dir.path().join("new.part"), dir.path().join("final.bin"));
    std::fs::write(&to, b"old").unwrap();
    std::fs::write(&from, b"new").unwrap();
    publish_file(&from, &to).unwrap();
    assert_eq!(std::fs::read(&to).unwrap(), b"new");
    assert!(!from.exists());
}

#[test]
fn publishes_across_directories_on_one_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let (from, to) = (dir.path().join("a.part"), sub.join("a.bin"));
    std::fs::write(&from, b"x").unwrap();
    publish_file(&from, &to).unwrap();
    assert_eq!(std::fs::read(&to).unwrap(), b"x");
}

#[test]
fn a_missing_source_fails_and_leaves_the_destination_alone() {
    let dir = tempfile::tempdir().unwrap();
    let to = dir.path().join("keep.bin");
    std::fs::write(&to, b"keep").unwrap();
    let err = publish_file(&dir.path().join("absent.part"), &to).unwrap_err();
    assert!(matches!(err, FsError::Io { .. }), "{err:?}");
    assert_eq!(std::fs::read(&to).unwrap(), b"keep");
}

#[test]
fn a_directory_source_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("d");
    std::fs::create_dir(&from).unwrap();
    let err = publish_file(&from, &dir.path().join("t")).unwrap_err();
    assert!(matches!(err, FsError::Io { .. }), "{err:?}");
    assert!(from.is_dir());
}

#[test]
fn no_replace_refuses_an_existing_destination_and_keeps_both_files() {
    let dir = tempfile::tempdir().unwrap();
    let (from, to) = (dir.path().join("n.part"), dir.path().join("n.bin"));
    std::fs::write(&to, b"old").unwrap();
    std::fs::write(&from, b"new").unwrap();
    let options = AtomicWriteOptions {
        replace: ReplaceMode::NoReplace,
        ..AtomicWriteOptions::default()
    };
    let err = publish_file_with(&from, &to, options).unwrap_err();
    assert!(matches!(err, FsError::AlreadyExists(_)), "{err:?}");
    assert_eq!(std::fs::read(&to).unwrap(), b"old");
    assert_eq!(std::fs::read(&from).unwrap(), b"new");
}

#[test]
fn create_parents_makes_the_destination_directory() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("p.part");
    let to = dir.path().join("a").join("b").join("p.bin");
    std::fs::write(&from, b"p").unwrap();
    let options = AtomicWriteOptions {
        create_parents: true,
        ..AtomicWriteOptions::default()
    };
    publish_file_with(&from, &to, options).unwrap();
    assert_eq!(std::fs::read(&to).unwrap(), b"p");
}

#[test]
fn a_generated_temp_shaped_destination_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("r.part");
    std::fs::write(&from, b"r").unwrap();
    let err = publish_file(&from, &dir.path().join(".rec.rktmp-1-1-0")).unwrap_err();
    assert!(matches!(err, FsError::BadName(_)), "{err:?}");
    assert!(from.exists());
}

#[cfg(unix)]
#[test]
fn a_symlink_source_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::write(&real, b"r").unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(publish_file(&link, &dir.path().join("t")).is_err());
}
