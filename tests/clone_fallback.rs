//! `clone_or_link_or_copy_nofollow`: the clone, hard-link and copy steps, each produced
//! deterministically through the `clone::testing` step seam; an existing destination and a link at
//! either end are refused; the copy goes through the durable atomic-write path.

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rightkit_fs::atomic::testing::take_steps;
#[cfg(unix)]
use rightkit_fs::atomic::testing::Step;
use rightkit_fs::clone::testing::{override_clone, override_link};
use rightkit_fs::{
    clone_or_link_or_copy_nofollow, clone_or_link_or_copy_nofollow_with, CloneOptions,
    CloneOutcome, FsError,
};

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

fn source(dir: &Path) -> PathBuf {
    let src = dir.join("source.bin");
    std::fs::write(&src, b"source content").unwrap();
    src
}

fn unavailable(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

fn must_not_run(_: &Path, _: &Path) -> io::Result<()> {
    panic!("this step must not run");
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[cfg(unix)]
fn link(target: &Path, at: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(windows)]
fn link(target: &Path, at: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, at)
}

// ------------------------------------------------------------------ each outcome

#[test]
fn a_working_clone_step_wins_and_the_later_steps_never_run() {
    let (_g, dir) = tmp();
    let (src, dst) = (source(&dir), dir.join("clone.bin"));
    let clones = Rc::new(Cell::new(0));
    let counter = clones.clone();
    let _clone = override_clone(move |from, to| {
        counter.set(counter.get() + 1);
        std::fs::copy(from, to).map(|_| ())
    });
    let _link = override_link(must_not_run);
    let outcome = clone_or_link_or_copy_nofollow(&src, &dst).unwrap();
    assert_eq!(outcome, CloneOutcome::Cloned);
    assert_eq!(clones.get(), 1);
    assert_eq!(std::fs::read(&dst).unwrap(), b"source content");
}

#[test]
fn an_unavailable_clone_falls_through_to_a_hard_link_that_shares_content() {
    let (_g, dir) = tmp();
    let (src, dst) = (source(&dir), dir.join("linked.bin"));
    let _clone = override_clone(unavailable);
    let outcome = clone_or_link_or_copy_nofollow(&src, &dst).unwrap();
    assert_eq!(outcome, CloneOutcome::HardLinked);
    assert_eq!(std::fs::read(&dst).unwrap(), b"source content");
    // The documented caveat: one inode, so an in-place write through either name changes both.
    std::fs::write(&dst, b"changed in place").unwrap();
    assert_eq!(std::fs::read(&src).unwrap(), b"changed in place");
    // And removing the linked name leaves the source's content.
    std::fs::remove_file(&dst).unwrap();
    assert_eq!(std::fs::read(&src).unwrap(), b"changed in place");
}

#[test]
fn when_clone_and_link_are_unavailable_the_copy_is_independent() {
    let (_g, dir) = tmp();
    let (src, dst) = (source(&dir), dir.join("copy.bin"));
    let _clone = override_clone(unavailable);
    let _link = override_link(unavailable);
    let _ = take_steps();
    let outcome = clone_or_link_or_copy_nofollow(&src, &dst).unwrap();
    assert_eq!(outcome, CloneOutcome::Copied);
    assert_eq!(std::fs::read(&dst).unwrap(), b"source content");
    std::fs::write(&dst, b"changed in place").unwrap();
    assert_eq!(
        std::fs::read(&src).unwrap(),
        b"source content",
        "a copy has its own storage"
    );
    assert_eq!(names(&dir), vec!["copy.bin", "source.bin"], "no temp left");
}

#[cfg(unix)]
#[test]
fn the_copy_keeps_the_permission_bits_and_uses_the_durable_write_path() {
    use std::os::unix::fs::PermissionsExt;
    let (_g, dir) = tmp();
    let (src, dst) = (source(&dir), dir.join("copy.bin"));
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).unwrap();
    let _clone = override_clone(unavailable);
    let _link = override_link(unavailable);
    let _ = take_steps();
    assert_eq!(
        clone_or_link_or_copy_nofollow(&src, &dst).unwrap(),
        CloneOutcome::Copied
    );
    assert_eq!(
        std::fs::metadata(&dst).unwrap().permissions().mode() & 0o7777,
        0o640
    );
    assert_eq!(
        take_steps(),
        vec![
            Step::TempFlushed,
            Step::Replace,
            Step::DirSynced(dir.clone())
        ],
        "temp flushed, published exclusively, directory synced"
    );
}

#[cfg(unix)]
#[test]
fn a_clone_or_link_syncs_the_directory_entry_it_made_when_durable() {
    let (_g, dir) = tmp();
    let src = source(&dir);
    let _clone = override_clone(unavailable);
    let _ = take_steps();
    let dst = dir.join("linked.bin");
    assert_eq!(
        clone_or_link_or_copy_nofollow(&src, &dst).unwrap(),
        CloneOutcome::HardLinked
    );
    assert_eq!(take_steps(), vec![Step::DirSynced(dir.clone())]);

    let quiet = CloneOptions {
        durable: false,
        ..CloneOptions::default()
    };
    let _ = take_steps();
    clone_or_link_or_copy_nofollow_with(&src, &dir.join("linked2.bin"), &quiet).unwrap();
    assert!(take_steps().is_empty(), "not durable: nothing is synced");
}

#[test]
fn disallowing_the_hard_link_goes_straight_to_a_copy() {
    let (_g, dir) = tmp();
    let (src, dst) = (source(&dir), dir.join("copy.bin"));
    let _clone = override_clone(unavailable);
    let _link = override_link(must_not_run);
    let options = CloneOptions::default().without_hard_link();
    assert!(!options.allow_hard_link);
    let outcome = clone_or_link_or_copy_nofollow_with(&src, &dst, &options).unwrap();
    assert_eq!(outcome, CloneOutcome::Copied);
    std::fs::write(&dst, b"modified").unwrap();
    assert_eq!(
        std::fs::read(&src).unwrap(),
        b"source content",
        "the caller that will modify the copy is not sharing the source's inode"
    );
}

#[test]
fn the_default_ladder_works_on_this_platform_whatever_step_it_lands_on() {
    let (_g, dir) = tmp();
    let src = source(&dir);
    let dst = dir.join("any.bin");
    let outcome = clone_or_link_or_copy_nofollow(&src, &dst).unwrap();
    assert!(matches!(
        outcome,
        CloneOutcome::Cloned | CloneOutcome::HardLinked | CloneOutcome::Copied
    ));
    assert_eq!(std::fs::read(&dst).unwrap(), b"source content");

    let other = dir.join("unlinked.bin");
    let outcome = clone_or_link_or_copy_nofollow_with(
        &src,
        &other,
        &CloneOptions::default().without_hard_link(),
    )
    .unwrap();
    assert_ne!(outcome, CloneOutcome::HardLinked);
    assert_eq!(std::fs::read(&other).unwrap(), b"source content");
}

// ------------------------------------------------------------------ refusals

#[test]
fn an_existing_destination_is_refused_before_any_step_runs() {
    let (_g, dir) = tmp();
    let (src, dst) = (source(&dir), dir.join("taken.bin"));
    std::fs::write(&dst, b"keep").unwrap();
    let _clone = override_clone(must_not_run);
    let _link = override_link(must_not_run);
    let error = clone_or_link_or_copy_nofollow(&src, &dst).expect_err("must not overwrite");
    assert!(
        matches!(&error, FsError::AlreadyExists(p) if p == &dst),
        "{error:?}"
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"keep");
}

#[test]
fn a_destination_that_appears_first_is_not_overwritten_by_a_later_step() {
    let (_g, dir) = tmp();
    let (src, dst) = (source(&dir), dir.join("raced.bin"));
    let _clone = override_clone(|_, to| {
        // Another writer won the name between the check and the step.
        std::fs::write(to, b"winner")?;
        Err(io::Error::from(io::ErrorKind::AlreadyExists))
    });
    let _link = override_link(must_not_run);
    let error = clone_or_link_or_copy_nofollow(&src, &dst).expect_err("lost the race");
    assert!(matches!(error, FsError::AlreadyExists(_)), "{error:?}");
    assert_eq!(std::fs::read(&dst).unwrap(), b"winner");
}

#[test]
fn a_symlink_source_is_refused_not_followed_and_creates_nothing() {
    let (_g, dir) = tmp();
    let real = source(&dir);
    let src = dir.join("link.bin");
    if let Err(e) = link(&real, &src) {
        eprintln!("SKIP: cannot create a symlink here: {e}");
        return;
    }
    let dst = dir.join("clone.bin");
    let _clone = override_clone(must_not_run);
    let _link = override_link(must_not_run);
    let error = clone_or_link_or_copy_nofollow(&src, &dst).expect_err("links are refused");
    assert!(
        matches!(&error, FsError::Symlink(p) if p == &src),
        "{error:?}"
    );
    assert!(!dst.exists());
}

#[test]
fn a_symlink_destination_is_refused_dangling_or_not_and_its_target_is_untouched() {
    let (_g, dir) = tmp();
    let src = source(&dir);
    let _clone = override_clone(must_not_run);
    let _link = override_link(must_not_run);

    // A link to a missing file: writing through it would create the target.
    let missing_target = dir.join("not-yet.bin");
    let dangling = dir.join("dangling.bin");
    if let Err(e) = link(&missing_target, &dangling) {
        eprintln!("SKIP: cannot create a symlink here: {e}");
        return;
    }
    let error = clone_or_link_or_copy_nofollow(&src, &dangling).expect_err("dangling link");
    assert!(matches!(error, FsError::Symlink(_)), "{error:?}");
    assert!(!missing_target.exists(), "nothing was created through it");

    // A link to an existing file: writing through it would overwrite the target.
    let victim = dir.join("victim.bin");
    std::fs::write(&victim, b"victim").unwrap();
    let to_file = dir.join("to-file.bin");
    link(&victim, &to_file).unwrap();
    let error = clone_or_link_or_copy_nofollow(&src, &to_file).expect_err("link to a file");
    assert!(matches!(error, FsError::Symlink(_)), "{error:?}");
    assert_eq!(std::fs::read(&victim).unwrap(), b"victim");
}

#[test]
fn a_directory_source_and_a_missing_source_are_typed_errors() {
    let (_g, dir) = tmp();
    let folder = dir.join("folder");
    std::fs::create_dir(&folder).unwrap();
    let error = clone_or_link_or_copy_nofollow(&folder, &dir.join("x")).unwrap_err();
    assert!(matches!(error, FsError::NotAFile(_)), "{error:?}");

    let error = clone_or_link_or_copy_nofollow(&dir.join("absent"), &dir.join("y")).unwrap_err();
    assert!(error.is_not_found(), "{error:?}");
    assert!(!dir.join("x").exists() && !dir.join("y").exists());
}

// ------------------------------------------------------------------ fresh metadata

#[cfg(unix)]
#[test]
fn without_fresh_metadata_a_read_only_source_gives_a_read_only_copy() {
    use std::os::unix::fs::PermissionsExt;
    let (_g, dir) = tmp();
    let (src, dst) = (source(&dir), dir.join("copy.bin"));
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o444)).unwrap();
    let _clone = override_clone(unavailable);
    let _link = override_link(unavailable);
    assert_eq!(
        clone_or_link_or_copy_nofollow(&src, &dst).unwrap(),
        CloneOutcome::Copied
    );
    assert_eq!(
        std::fs::metadata(&dst).unwrap().permissions().mode() & 0o7777,
        0o444,
        "default options keep the source's mode"
    );
}

#[cfg(unix)]
#[test]
fn fresh_metadata_gives_a_writable_copy_of_a_read_only_source() {
    use std::os::unix::fs::PermissionsExt;
    let (_g, dir) = tmp();
    let (src, dst) = (source(&dir), dir.join("copy.bin"));
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o444)).unwrap();
    let _clone = override_clone(unavailable);
    let _link = override_link(unavailable);
    let options = CloneOptions::default().fresh_metadata(true);
    assert_eq!(
        clone_or_link_or_copy_nofollow_with(&src, &dst, &options).unwrap(),
        CloneOutcome::Copied
    );
    let mode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o7777;
    assert_ne!(
        mode & 0o200,
        0,
        "the export is writable by its owner, mode {mode:o}"
    );
    assert_ne!(
        mode, 0o444,
        "the source's read-only mode is not carried over"
    );
    std::fs::write(&dst, b"edited export").unwrap();
    assert_eq!(std::fs::read(&src).unwrap(), b"source content");
}

#[cfg(unix)]
#[test]
fn fresh_metadata_skips_the_clone_and_link_steps_and_makes_a_new_inode() {
    use std::os::unix::fs::MetadataExt;
    let (_g, dir) = tmp();
    let (src, dst) = (source(&dir), dir.join("copy.bin"));
    let clone_calls = Rc::new(Cell::new(0usize));
    let link_calls = Rc::new(Cell::new(0usize));
    let (c, l) = (clone_calls.clone(), link_calls.clone());
    let _clone = override_clone(move |a, b| {
        c.set(c.get() + 1);
        must_not_run(a, b)
    });
    let _link = override_link(move |a, b| {
        l.set(l.get() + 1);
        must_not_run(a, b)
    });
    let options = CloneOptions::default().fresh_metadata(true);
    assert!(options.fresh_metadata);
    assert_eq!(
        clone_or_link_or_copy_nofollow_with(&src, &dst, &options).unwrap(),
        CloneOutcome::Copied
    );
    assert_eq!(clone_calls.get(), 0, "the clone step is not attempted");
    assert_eq!(link_calls.get(), 0, "the hard-link step is never used");
    assert_ne!(
        std::fs::metadata(&src).unwrap().ino(),
        std::fs::metadata(&dst).unwrap().ino(),
        "the export is not the source's inode"
    );
}

/// The copy of a private source never sits in a wider temp. The bytes are written into a file that
/// is created `0600` and given the source's bits only at the end, before publication; the mode
/// seen by the replace hook is the source's, and the destination ends with it. (The window itself
/// is not observable from a test; the temp's creation mode is fixed by the write options.)
#[cfg(unix)]
#[test]
fn a_private_source_is_copied_through_an_owner_only_temp_and_ends_private() {
    use rightkit_fs::atomic::testing::on_replace;
    use std::os::unix::fs::PermissionsExt;
    let (_g, dir) = tmp();
    let (src, dst) = (source(&dir), dir.join("copy.bin"));
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o600)).unwrap();
    let _clone = override_clone(unavailable);
    let _link = override_link(unavailable);
    let at_publication = Rc::new(Cell::new(0u32));
    let sink = at_publication.clone();
    let _replace = on_replace(move |call| {
        sink.set(std::fs::metadata(&call.temp).unwrap().permissions().mode() & 0o7777);
        None
    });
    assert_eq!(
        clone_or_link_or_copy_nofollow(&src, &dst).unwrap(),
        CloneOutcome::Copied
    );
    assert_eq!(at_publication.get(), 0o600, "temp mode when published");
    assert_eq!(
        std::fs::metadata(&dst).unwrap().permissions().mode() & 0o7777,
        0o600
    );
}

/// A source with no group or other permission bit is never given to the native macOS clone:
/// `clonefile` creates the destination itself, with the source's mode and whatever ACL the
/// destination directory hands out, so owner-only cannot be established. No seam is installed, so
/// the real clone step runs (on APFS it would clone a source readable by others). With the hard
/// link disallowed the ladder lands on the copy, which creates its destination `0600` first.
#[cfg(target_os = "macos")]
#[test]
fn a_private_source_is_never_cloned_natively_on_macos() {
    use std::os::unix::fs::PermissionsExt;
    let (_g, dir) = tmp();
    let (src, dst) = (source(&dir), dir.join("copy.bin"));
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o600)).unwrap();
    let outcome = clone_or_link_or_copy_nofollow_with(
        &src,
        &dst,
        &CloneOptions::default().without_hard_link(),
    )
    .unwrap();
    assert_eq!(
        outcome,
        CloneOutcome::Copied,
        "a private source is copied, not cloned"
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"source content");
    assert_eq!(
        std::fs::metadata(&dst).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    // The default ladder may still hard-link it (the same inode, with its own mode and ACL, so
    // nothing new is exposed), but it never clones it.
    let linked = dir.join("linked.bin");
    let outcome = clone_or_link_or_copy_nofollow(&src, &linked).unwrap();
    assert_ne!(outcome, CloneOutcome::Cloned);
}

/// The permissions a copy ends with come from the file it actually read. The clone step runs
/// after the source was checked by path and before the copy opens it; here it replaces the source
/// in that gap (a `0644` file by a `0600` secret, and the reverse). The copy must carry the mode
/// of the opened file, not the one of the file that used to be at the path: the old code applied
/// the stale mode and published a secret `0644`.
#[cfg(unix)]
#[test]
fn a_source_replaced_between_the_check_and_the_copy_is_copied_with_its_own_permissions() {
    use std::os::unix::fs::PermissionsExt;
    for (before, after) in [(0o644, 0o600), (0o600, 0o644)] {
        let (_g, dir) = tmp();
        let (src, dst) = (source(&dir), dir.join("copy.bin"));
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(before)).unwrap();
        let replaced = src.clone();
        let _clone = override_clone(move |_, _| {
            std::fs::remove_file(&replaced).unwrap();
            std::fs::write(&replaced, b"replacement content").unwrap();
            std::fs::set_permissions(&replaced, std::fs::Permissions::from_mode(after)).unwrap();
            Err(io::Error::from(io::ErrorKind::Unsupported))
        });
        let _link = override_link(unavailable);
        assert_eq!(
            clone_or_link_or_copy_nofollow(&src, &dst).unwrap(),
            CloneOutcome::Copied
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"replacement content");
        assert_eq!(
            std::fs::metadata(&dst).unwrap().permissions().mode() & 0o7777,
            after,
            "source went {before:o} -> {after:o}: the copy takes the replacement's mode"
        );
    }
}

#[test]
fn a_destination_named_like_a_generated_temp_file_is_refused() {
    let (_g, dir) = tmp();
    let src = source(&dir);
    let dst = dir.join(".copy.bin.rktmp-1-2-0");
    let error = clone_or_link_or_copy_nofollow(&src, &dst).expect_err("reserved name");
    assert!(matches!(error, FsError::BadName(_)), "{error:?}");
    assert!(!dst.exists());
}
