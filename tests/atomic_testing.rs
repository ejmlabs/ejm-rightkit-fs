//! The `testing` seam around the replace call: the flags each mode hands the OS, and an injected
//! failure that leaves the old file intact and the temp removed.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use rightkit_fs::atomic::testing::on_replace;
use rightkit_fs::atomic::{
    ReplaceApi, ReplaceCall, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};
use rightkit_fs::{atomic_write_with_options, AtomicWriteOptions, ReplaceMode};

fn write(
    path: &std::path::Path,
    mode: ReplaceMode,
    durable: bool,
    private: bool,
    bytes: &[u8],
) -> rightkit_fs::Result<()> {
    let options = AtomicWriteOptions {
        replace: mode,
        durable,
        private,
        ..AtomicWriteOptions::default()
    };
    atomic_write_with_options(path, options, |f| f.write_all(bytes))
}

fn capture(path: &std::path::Path, mode: ReplaceMode, durable: bool) -> ReplaceCall {
    let seen: Rc<RefCell<Vec<ReplaceCall>>> = Rc::default();
    let sink = seen.clone();
    let _guard = on_replace(move |call| {
        sink.borrow_mut().push(call.clone());
        None
    });
    write(path, mode, durable, false, b"x").unwrap();
    let calls = seen.borrow();
    assert_eq!(calls.len(), 1, "one replace call per write");
    calls[0].clone()
}

#[test]
fn the_flags_each_mode_hands_the_os() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("existing.bin");
    std::fs::write(&existing, b"old").unwrap();

    for durable in [true, false] {
        let rename = capture(&existing, ReplaceMode::Rename, durable);
        let moved = capture(&existing, ReplaceMode::MoveWriteThrough, durable);
        let missing = dir.path().join(format!("missing-{durable}.bin"));
        let moved_to_missing = capture(&missing, ReplaceMode::MoveWriteThrough, durable);
        let exclusive_target = dir.path().join(format!("exclusive-{durable}.bin"));
        let exclusive = capture(&exclusive_target, ReplaceMode::NoReplace, durable);
        assert_eq!(
            std::fs::read(&missing).unwrap(),
            b"x",
            "the write still lands"
        );

        if cfg!(windows) {
            assert_eq!(exclusive.api, ReplaceApi::MoveFileEx);
            assert_eq!(
                exclusive.flags,
                if durable { MOVEFILE_WRITE_THROUGH } else { 0 },
                "NoReplace must not pass MOVEFILE_REPLACE_EXISTING, durable={durable}"
            );
            // Rename stays std::fs::rename (POSIX replace semantics); only MoveWriteThrough moves with
            // MoveFileExW.
            assert_eq!(
                (rename.api, rename.flags),
                (ReplaceApi::Rename, 0),
                "Rename, durable={durable}"
            );
            assert_eq!(moved.api, ReplaceApi::MoveFileEx);
            assert_eq!(
                moved.flags,
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                "MoveWriteThrough, durable={durable}"
            );
            assert_eq!(moved_to_missing.api, ReplaceApi::MoveFileEx);
            assert_eq!(
                moved_to_missing.flags,
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                "MoveWriteThrough over a missing destination, durable={durable}"
            );
        } else {
            for call in [&rename, &moved, &moved_to_missing] {
                assert_eq!((call.api, call.flags), (ReplaceApi::Rename, 0));
            }
            assert_eq!(
                (exclusive.api, exclusive.flags),
                (ReplaceApi::RenameExclusive, 0)
            );
        }
    }
}

#[test]
fn an_injected_replace_failure_leaves_the_old_file_and_removes_the_temp() {
    for mode in [ReplaceMode::Rename, ReplaceMode::MoveWriteThrough] {
        for private in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("state.json");
            std::fs::write(&target, b"old").unwrap();

            let temp_seen = Rc::new(RefCell::new(None));
            let sink = temp_seen.clone();
            let result = {
                let _guard = on_replace(move |call| {
                    *sink.borrow_mut() = Some(call.temp.clone());
                    Some(std::io::Error::other("injected replace failure"))
                });
                write(&target, mode, true, private, b"new")
            };
            let error = result.expect_err("the injected failure must surface");
            assert!(
                error.to_string().contains("injected replace failure"),
                "{error}"
            );
            assert_eq!(
                std::fs::read(&target).unwrap(),
                b"old",
                "{mode:?} private={private}: old file intact"
            );
            let temp = temp_seen
                .borrow()
                .clone()
                .expect("the hook saw the temp path");
            assert!(!temp.exists(), "{mode:?} private={private}: temp removed");
            let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
            assert_eq!(entries.len(), 1, "only the destination remains");

            // The hook is gone with its guard: the next write succeeds.
            write(&target, mode, true, private, b"new").unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), b"new");
        }
    }
}
