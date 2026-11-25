use eon_foundation::journal::{JournalWriter, replay_from};
use eon_foundation::store::StateStore;

#[test]
fn restart_restores_snapshot_then_replays_only_the_missing_records() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let journal = dir.path().join("journal");
    let mut writer = JournalWriter::open(&journal).unwrap();
    let seqs = writer.append_batch(&[b"a", b"b", b"c", b"d", b"e"]).unwrap();
    let mut store = StateStore::open(&db).unwrap();
    store.commit_snapshot("letters", seqs[1], b"ab").unwrap();
    store.advance_checkpoint("letters", seqs[3]).unwrap();
    drop(store);
    drop(writer);

    let mut store = StateStore::open(db).unwrap();
    let snapshot = store.snapshot("letters").unwrap().unwrap();
    assert_eq!(snapshot.journal_seq, seqs[1]);
    let mut state = snapshot.state;
    let mut through = snapshot.journal_seq;
    for frame in replay_from(journal, snapshot.journal_seq + 1).unwrap() {
        through = frame.seq_no;
        state.extend(frame.payload);
    }
    store.commit_snapshot("letters", through, &state).unwrap();
    let restored = store.snapshot("letters").unwrap().unwrap();
    assert_eq!(restored.state, b"abcde");
    assert_eq!(restored.journal_seq, seqs[4]);
    assert_eq!(
        Some(restored.journal_seq),
        store.checkpoint("letters").unwrap()
    );
}

#[test]
fn existing_checkpoint_database_upgrades_without_inventing_a_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let old = rusqlite::Connection::open(&db).unwrap();
    old.execute_batch(
        "CREATE TABLE projection_checkpoint (name TEXT PRIMARY KEY, journal_seq INTEGER NOT NULL);
         INSERT INTO projection_checkpoint VALUES ('ui', 12);",
    )
    .unwrap();
    drop(old);

    let mut store = StateStore::open(db).unwrap();
    assert_eq!(store.snapshot("ui").unwrap(), None);
    assert!(store.commit_snapshot("ui", 11, b"stale").is_err());
    store.commit_snapshot("ui", 12, b"current").unwrap();
    store.commit_snapshot("ui", 12, b"current").unwrap();
    assert!(store.commit_snapshot("ui", 12, b"conflict").is_err());
    assert_eq!(store.snapshot("ui").unwrap().unwrap().state, b"current");
    store.commit_snapshot("other", 0, b"").unwrap();
    assert_eq!(store.checkpoint("other").unwrap(), Some(0));
    assert_eq!(store.checkpoint("ui").unwrap(), Some(12));
}
