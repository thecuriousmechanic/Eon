#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

use eon_foundation::journal::{JournalWriter, repair_torn_tail, replay};

const FIRST: &str = "seg_00000000000000000001.eonj";

fn private_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}

#[test]
fn refuse_linked_segments_without_modifying_a_sentinel() {
    for hard_link in [false, true] {
        let root = private_dir();
        let journal = root.path().join("journal");
        drop(JournalWriter::open(&journal).unwrap());
        let segment = journal.join(FIRST);
        fs::remove_file(&segment).unwrap();
        let sentinel = root.path().join("sentinel");
        fs::write(&sentinel, b"preserve").unwrap();
        fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o600)).unwrap();
        if hard_link {
            fs::hard_link(&sentinel, &segment).unwrap();
        } else {
            symlink(&sentinel, &segment).unwrap();
        }
        let before = fs::metadata(&sentinel).unwrap();
        assert!(JournalWriter::open(&journal).is_err());
        assert!(repair_torn_tail(&journal).is_err());
        assert!(replay(&journal).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserve");
        let after = fs::metadata(&sentinel).unwrap();
        assert_eq!((after.mode(), after.ino()), (before.mode(), before.ino()));
    }
}

#[test]
fn unsafe_ancestor_is_refused_before_any_file_is_created() {
    let root = private_dir();
    let shared = root.path().join("mutable");
    let journal = shared.join("private");
    fs::create_dir_all(&journal).unwrap();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(JournalWriter::open(&journal).is_err());
    let alias = root.path().join("alias");
    symlink(&journal, &alias).unwrap();
    assert!(JournalWriter::open(&alias).is_err());
    assert_eq!(fs::read_dir(&journal).unwrap().count(), 0);
}

#[test]
fn rotation_stays_at_the_pinned_directory_after_a_name_replacement() {
    let root = private_dir();
    let path = root.path().join("journal");
    let mut writer = JournalWriter::open_with_segment_size(&path, 1).unwrap();
    writer.append_batch(&[b"first"]).unwrap();
    let archived = root.path().join("original");
    fs::rename(&path, &archived).unwrap();
    fs::create_dir(&path).unwrap();
    let sentinel = path.join(FIRST);
    fs::write(&sentinel, b"other directory").unwrap();
    writer.append_batch(&[b"second"]).unwrap();
    drop(writer);
    assert_eq!(replay(&archived).unwrap().len(), 2);
    assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
    assert_eq!(fs::read(sentinel).unwrap(), b"other directory");
}

#[test]
fn a_leftover_link_at_the_index_work_path_is_never_truncated() {
    let root = private_dir();
    let mut writer = JournalWriter::open_with_segment_size(root.path(), 1).unwrap();
    writer.append_batch(&[b"first"]).unwrap();
    let sentinel = root.path().join("sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();
    symlink(&sentinel, root.path().join(FIRST).with_extension("idx.tmp")).unwrap();
    assert!(writer.append_batch(&[b"second"]).is_err());
    assert_eq!(fs::read(sentinel).unwrap(), b"unchanged");
}
