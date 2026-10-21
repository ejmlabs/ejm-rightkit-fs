//! G9: exact CutRight STORED encoding, deterministic metadata & malformed-input checks.
use rightkit_fs::zip::{read_zip, write_zip};
use std::io::ErrorKind;

// CutRight zip.rs encoding: alpha + empty UTF-8 entry, fixed epoch & Unix 0644.
const CUTRIGHT_HEX: &str = "504b0304140000080000000021006a39e0d0050000000500000009000000706b672f612e747874616c706861504b0304140000080000000021000000000000000000000000000a000000706b672fc3a92e62696e504b01021e03140000080000000021006a39e0d00500000005000000090000000000000000000000a48100000000706b672f612e747874504b01021e03140000080000000021000000000000000000000000000a0000000000000000000000a4812c000000706b672fc3a92e62696e504b050600000000020002006f000000540000000000";

fn fixture() -> Vec<u8> {
    (0..CUTRIGHT_HEX.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&CUTRIGHT_HEX[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn output_matches_cutright_bytes_including_utf8_empty_entry_and_order() {
    let d = tempfile::tempdir().unwrap();
    let a = d.path().join("a");
    let b = d.path().join("b");
    std::fs::write(&a, b"alpha").unwrap();
    std::fs::write(&b, b"").unwrap();
    let entries = vec![("pkg/a.txt".into(), a.clone()), ("pkg/é.bin".into(), b)];
    let dest = d.path().join("first.zip");
    write_zip(&dest, &entries).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), fixture());
    assert_eq!(
        read_zip(&dest).unwrap(),
        [
            ("pkg/a.txt".into(), b"alpha".to_vec()),
            ("pkg/é.bin".into(), vec![])
        ]
    );
    // Different filesystem metadata must not alter output.
    let file = std::fs::OpenOptions::new().write(true).open(&a).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let dest2 = d.path().join("second.zip");
    write_zip(&dest2, &entries).unwrap();
    assert_eq!(std::fs::read(dest2).unwrap(), fixture());
    let mut reversed = entries;
    reversed.reverse();
    write_zip(&dest, &reversed).unwrap();
    assert_ne!(std::fs::read(dest).unwrap(), fixture());
}

#[test]
fn empty_archive_is_exact_eocd() {
    let d = tempfile::tempdir().unwrap();
    let dest = d.path().join("empty.zip");
    write_zip(&dest, &[]).unwrap();
    let mut expected = vec![0x50, 0x4b, 0x05, 0x06];
    expected.resize(22, 0);
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    assert!(read_zip(&dest).unwrap().is_empty());
}

#[test]
fn zip_limits_fail_before_truncating_destination() {
    let d = tempfile::tempdir().unwrap();
    let dest = d.path().join("old.zip");
    std::fs::write(&dest, b"old").unwrap();
    let missing = d.path().join("missing");
    assert_eq!(
        write_zip(&dest, &[("x".repeat(65536), missing.clone())])
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        write_zip(&dest, &vec![("x".into(), missing); 65535])
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"old");
}

#[test]
fn maximum_name_length_and_duplicate_names_round_trip() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    let dest = d.path().join("out.zip");
    std::fs::write(&src, b"abc").unwrap();
    let name = "x".repeat(65535);
    write_zip(&dest, &[(name.clone(), src.clone()), (name.clone(), src)]).unwrap();
    assert_eq!(
        read_zip(&dest).unwrap(),
        [(name.clone(), b"abc".to_vec()), (name, b"abc".to_vec())]
    );
}

#[test]
fn truncation_crc_damage_and_hostile_offsets_return_errors_without_panics() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("bad.zip");
    let good = fixture();
    for end in 0..good.len() {
        std::fs::write(&path, &good[..end]).unwrap();
        assert!(read_zip(&path).is_err(), "truncation {end}");
    }
    let mut bad = good.clone();
    bad[39] ^= 1; // First payload byte.
    std::fs::write(&path, &bad).unwrap();
    assert_eq!(read_zip(&path).unwrap_err().kind(), ErrorKind::InvalidData);
    // EOCD central offset; central local offset, name length & size fields.
    let central =
        u32::from_le_bytes(good[good.len() - 6..good.len() - 2].try_into().unwrap()) as usize;
    for (offset, width) in [
        (good.len() - 6, 4),
        (central + 42, 4),
        (central + 28, 2),
        (central + 24, 4),
    ] {
        let mut bad = good.clone();
        bad[offset..offset + width].fill(0xff);
        std::fs::write(&path, &bad).unwrap();
        assert_eq!(read_zip(&path).unwrap_err().kind(), ErrorKind::InvalidData);
    }
}

#[test]
fn missing_source_and_destination_parent_return_io_errors() {
    let d = tempfile::tempdir().unwrap();
    let missing = d.path().join("missing");
    assert_eq!(
        write_zip(&d.path().join("out.zip"), &[("x".into(), missing.clone())])
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        write_zip(&missing.join("out.zip"), &[]).unwrap_err().kind(),
        ErrorKind::NotFound
    );
}
