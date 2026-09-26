use std::fs;
use trajfs_core::hash::sha_of_bytes;
use trajfs_core::pack::{PackReader, PackWriter};

#[test]
fn malformed_locations_fail_before_allocation_or_cached_reads() {
    let temp = tempfile::tempdir().unwrap();
    let mut writer = PackWriter::new(temp.path(), 1).unwrap();
    writer.add_bytes(sha_of_bytes(b"hello"), b"hello").unwrap();
    let (rows, _, _, _) = writer.finish().unwrap();
    let loc = rows[0].loc;
    let mut reader = PackReader::new(temp.path());
    assert_eq!(reader.blob(&[loc]).unwrap(), b"hello");
    for bad in [
        trajfs_core::pack::Loc {
            chunk_len: -1,
            ..loc
        },
        trajfs_core::pack::Loc {
            chunk_offset: -1,
            ..loc
        },
        trajfs_core::pack::Loc {
            chunk_len: loc.chunk_len + 1,
            ..loc
        },
        trajfs_core::pack::Loc { offset: -1, ..loc },
        trajfs_core::pack::Loc { size: -1, ..loc },
        trajfs_core::pack::Loc {
            size: i64::MAX,
            ..loc
        },
    ] {
        assert!(reader.blob(&[bad]).is_err());
        assert!(reader.copy_blob(&[bad], &mut Vec::new()).is_err());
    }
}

#[test]
fn immutable_pack_and_existing_temporary_paths_are_never_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("0001.pack"), b"retained pack").unwrap();
    let mut writer = PackWriter::new(temp.path(), 1).unwrap();
    writer.add_bytes(sha_of_bytes(b"new"), b"new").unwrap();
    assert!(writer.finish().is_err());
    assert_eq!(
        fs::read(temp.path().join("0001.pack")).unwrap(),
        b"retained pack"
    );

    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("sentinel");
    fs::write(&outside, b"untouched").unwrap();
    std::os::unix::fs::symlink(&outside, temp.path().join("0001.pack.tmp")).unwrap();
    let mut writer = PackWriter::new(temp.path(), 1).unwrap();
    writer.add_bytes(sha_of_bytes(b"new"), b"new").unwrap();
    assert!(writer.finish().is_err());
    assert_eq!(fs::read(outside).unwrap(), b"untouched");
}

#[test]
fn incorrect_supplied_hash_is_not_written() {
    let temp = tempfile::tempdir().unwrap();
    let mut writer = PackWriter::new(temp.path(), 1).unwrap();
    assert!(writer
        .add_bytes(sha_of_bytes(b"different"), b"hello")
        .is_err());
    assert!(PackWriter::new(temp.path(), 0).is_err());
}
