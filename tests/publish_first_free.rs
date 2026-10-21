//! `publish_file_first_free`: one finished file published under the first free name of a candidate
//! list, exclusively, without being rewritten.

use std::path::PathBuf;

use rightkit_fs::{publish_file_first_free, AtomicWriteOptions, FsError};

fn options() -> AtomicWriteOptions {
    AtomicWriteOptions::default()
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut all: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    all.sort();
    all
}

#[test]
fn the_first_free_candidate_wins_and_taken_names_are_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("work.tmp");
    std::fs::write(&from, b"finished").unwrap();
    let candidates: Vec<PathBuf> = ["a.dng", "a-1.dng", "a-2.dng"]
        .iter()
        .map(|n| dir.path().join(n))
        .collect();
    std::fs::write(&candidates[0], b"taken-0").unwrap();
    std::fs::write(&candidates[1], b"taken-1").unwrap();

    let published = publish_file_first_free(&from, &candidates, options()).unwrap();
    assert_eq!(published, candidates[2]);
    assert_eq!(std::fs::read(&candidates[2]).unwrap(), b"finished");
    assert_eq!(std::fs::read(&candidates[0]).unwrap(), b"taken-0");
    assert_eq!(std::fs::read(&candidates[1]).unwrap(), b"taken-1");
    assert!(!from.exists(), "the finished file is moved, not copied");
}

#[test]
fn a_free_first_candidate_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("work.tmp");
    std::fs::write(&from, b"x").unwrap();
    let candidates = vec![dir.path().join("a.dng"), dir.path().join("a-1.dng")];
    let published = publish_file_first_free(&from, &candidates, options()).unwrap();
    assert_eq!(published, candidates[0]);
    assert_eq!(names(dir.path()), vec!["a.dng".to_string()]);
}

#[test]
fn when_every_candidate_exists_the_last_is_reported_and_the_source_stays() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("work.tmp");
    std::fs::write(&from, b"finished").unwrap();
    let candidates = vec![dir.path().join("a.dng"), dir.path().join("a-1.dng")];
    for c in &candidates {
        std::fs::write(c, b"taken").unwrap();
    }
    let error = publish_file_first_free(&from, &candidates, options()).unwrap_err();
    assert!(
        matches!(&error, FsError::AlreadyExists(p) if p == &candidates[1]),
        "{error:?}"
    );
    assert_eq!(std::fs::read(&from).unwrap(), b"finished");
}

#[test]
fn an_empty_candidate_list_is_invalid_input() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("work.tmp");
    std::fs::write(&from, b"x").unwrap();
    let error = publish_file_first_free(&from, &[], options()).unwrap_err();
    assert_eq!(error.kind(), Some(std::io::ErrorKind::InvalidInput));
    assert!(from.exists());
}

#[test]
fn a_reserved_candidate_name_stops_the_search() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("work.tmp");
    std::fs::write(&from, b"x").unwrap();
    let candidates = vec![
        dir.path().join(".a.rktmp-123-1-0"),
        dir.path().join("a.dng"),
    ];
    let error = publish_file_first_free(&from, &candidates, options()).unwrap_err();
    assert!(matches!(error, FsError::BadName(_)), "{error:?}");
    assert!(!candidates[1].exists(), "later candidates are not tried");
    assert!(from.exists());
}

#[test]
fn a_missing_source_is_an_error_not_a_skip() {
    let dir = tempfile::tempdir().unwrap();
    let candidates = vec![dir.path().join("a.dng")];
    let error =
        publish_file_first_free(&dir.path().join("nope"), &candidates, options()).unwrap_err();
    assert!(matches!(error, FsError::Io { .. }), "{error:?}");
    assert!(!candidates[0].exists());
}

#[test]
fn create_parents_makes_each_candidates_folder() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("work.tmp");
    std::fs::write(&from, b"x").unwrap();
    let candidates = vec![dir.path().join("out/deep/a.dng")];
    let options = AtomicWriteOptions {
        create_parents: true,
        ..options()
    };
    let published = publish_file_first_free(&from, &candidates, options).unwrap();
    assert_eq!(std::fs::read(published).unwrap(), b"x");
}

/// With no hard links and no exclusive rename the search still works (placeholder path).
#[cfg(all(unix, feature = "testing"))]
#[test]
fn the_search_works_on_a_volume_without_hard_links() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("work.tmp");
    std::fs::write(&from, b"finished").unwrap();
    let candidates = vec![dir.path().join("a.dng"), dir.path().join("a-1.dng")];
    std::fs::write(&candidates[0], b"taken").unwrap();
    let _no_links = rightkit_fs::atomic::testing::force_no_hard_links();
    let published = publish_file_first_free(&from, &candidates, options()).unwrap();
    assert_eq!(published, candidates[1]);
    assert_eq!(std::fs::read(&candidates[1]).unwrap(), b"finished");
    assert_eq!(std::fs::read(&candidates[0]).unwrap(), b"taken");
}
