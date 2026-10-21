//! M8 Unit 3 (Windows): the recycle operation is fail-closed at the public API. Until a
//! recycle-only shell operation is bound, `move_to_trash` must refuse and the item's bytes
//! must stay exactly where they were (never a permanent delete dressed as recycling).

#![cfg(all(windows, feature = "fs"))]

#[test]
fn move_to_trash_refuses_and_retains_the_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("keep.bin");
    let bytes: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&file, &bytes).unwrap();
    let err = rightkit_fs::move_to_trash(&file).unwrap_err();
    assert!(
        matches!(err, rightkit_fs::FsError::Unavailable(_)),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&file).unwrap(), bytes);

    let tree = dir.path().join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("sub").join("a.txt"), b"a").unwrap();
    assert!(matches!(
        rightkit_fs::move_to_trash(&tree),
        Err(rightkit_fs::FsError::Unavailable(_))
    ));
    assert_eq!(std::fs::read(tree.join("sub").join("a.txt")).unwrap(), b"a");
}
