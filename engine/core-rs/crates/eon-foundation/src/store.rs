//! SQLite projection store: WAL mode and `sqlite-vec` (docs/implementation/01, section 3.2).
//!
//! The journal owns durability. This store is a rebuildable projection, so `synchronous = NORMAL` is intentional.

use std::path::Path;
use std::sync::Once;

use eon_primitives::{EonError, EonResult};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

static REGISTER_VEC: Once = Once::new();

type AutoExtensionInit = unsafe extern "C" fn(
    *mut rusqlite::ffi::sqlite3,
    *mut *mut std::ffi::c_char,
    *const rusqlite::ffi::sqlite3_api_routines,
) -> std::ffi::c_int;

fn register_sqlite_vec() {
    REGISTER_VEC.call_once(|| unsafe {
        let init: AutoExtensionInit = std::mem::transmute::<*const (), AutoExtensionInit>(
            sqlite_vec::sqlite3_vec_init as *const (),
        );
        rusqlite::ffi::sqlite3_auto_extension(Some(init));
    });
}

pub struct StateStore {
    conn: Connection,
}

/// A projection's serialized state and the last journal record included in that state.
#[derive(Debug, PartialEq, Eq)]
pub struct ProjectionSnapshot {
    pub journal_seq: u64,
    pub state: Vec<u8>,
}

impl StateStore {
    pub fn open(path: impl AsRef<Path>) -> EonResult<Self> {
        register_sqlite_vec();
        let conn = Connection::open(path).map_err(sql_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;
             PRAGMA mmap_size = 134217728;
             CREATE TABLE IF NOT EXISTS projection_checkpoint (
                 name TEXT PRIMARY KEY,
                 journal_seq INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS projection_snapshot (
                 name TEXT PRIMARY KEY REFERENCES projection_checkpoint(name),
                 journal_seq INTEGER NOT NULL CHECK (journal_seq >= 0),
                 state BLOB NOT NULL
             );",
        )
        .map_err(sql_err)?;
        Ok(Self { conn })
    }

    pub fn journal_mode(&self) -> EonResult<String> {
        self.conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .map_err(sql_err)
    }

    pub fn vec_version(&self) -> EonResult<String> {
        self.conn
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .map_err(sql_err)
    }

    /// Records how far a named projection has applied the journal. Only moves forward.
    pub fn advance_checkpoint(&self, name: &str, journal_seq: u64) -> EonResult<()> {
        advance_checkpoint(&self.conn, name, sqlite_seq(journal_seq)?)
    }

    pub fn checkpoint(&self, name: &str) -> EonResult<Option<u64>> {
        checkpoint(&self.conn, name)
    }

    /// Commits the state and its checkpoint together. An identical retry is safe; an older
    /// checkpoint or different state for an already snapshotted sequence is rejected.
    pub fn commit_snapshot(&mut self, name: &str, journal_seq: u64, state: &[u8]) -> EonResult<()> {
        let seq = sqlite_seq(journal_seq)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        if checkpoint(&tx, name)?.is_some_and(|current| current > journal_seq) {
            return Err(EonError::ValidationError(
                "snapshot predates the projection checkpoint".into(),
            ));
        }
        advance_checkpoint(&tx, name, seq)?;
        let written = tx.execute(
            "INSERT INTO projection_snapshot (name, journal_seq, state) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET journal_seq = excluded.journal_seq, state = excluded.state
             WHERE projection_snapshot.journal_seq < excluded.journal_seq
                OR (projection_snapshot.journal_seq = excluded.journal_seq AND projection_snapshot.state = excluded.state)",
            params![name, seq, state],
        ).map_err(sql_err)?;
        if written == 0 {
            return Err(EonError::ValidationError(
                "conflicting snapshot for journal sequence".into(),
            ));
        }
        tx.commit().map_err(sql_err)
    }

    /// Returns the last snapshot with its own replay position, even if a checkpoint-only
    /// writer has since advanced. An existing checkpoint does not imply a snapshot exists.
    pub fn snapshot(&self, name: &str) -> EonResult<Option<ProjectionSnapshot>> {
        self.conn
            .query_row(
                "SELECT journal_seq, state FROM projection_snapshot WHERE name = ?1",
                [name],
                |row| {
                    Ok(ProjectionSnapshot {
                        journal_seq: row.get(0)?,
                        state: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(sql_err)
    }
}

fn sqlite_seq(seq: u64) -> EonResult<i64> {
    i64::try_from(seq).map_err(|_| {
        EonError::ValidationError("journal sequence exceeds SQLite INTEGER range".into())
    })
}

fn advance_checkpoint(conn: &Connection, name: &str, seq: i64) -> EonResult<()> {
    conn.execute(
        "INSERT INTO projection_checkpoint (name, journal_seq) VALUES (?1, ?2)
         ON CONFLICT(name) DO UPDATE SET journal_seq = excluded.journal_seq
         WHERE excluded.journal_seq > projection_checkpoint.journal_seq",
        params![name, seq],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn checkpoint(conn: &Connection, name: &str) -> EonResult<Option<u64>> {
    conn.query_row(
        "SELECT journal_seq FROM projection_checkpoint WHERE name = ?1",
        [name],
        |row| row.get(0),
    )
    .optional()
    .map_err(sql_err)
}

fn sql_err(error: rusqlite::Error) -> EonError {
    EonError::OperationFailed {
        operation: "state_store".to_string(),
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_in_wal_mode_with_vec_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path().join("state.db")).unwrap();
        assert_eq!(store.journal_mode().unwrap(), "wal");
        assert!(store.vec_version().unwrap().starts_with('v'));
    }

    #[test]
    fn checkpoint_only_moves_forward() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path().join("state.db")).unwrap();
        store.advance_checkpoint("ui", 10).unwrap();
        store.advance_checkpoint("ui", 4).unwrap();
        assert_eq!(store.checkpoint("ui").unwrap(), Some(10));
        assert_eq!(store.checkpoint("absent").unwrap(), None);
    }

    #[test]
    fn failed_snapshot_write_rolls_back_the_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = StateStore::open(dir.path().join("state.db")).unwrap();
        store.commit_snapshot("ui", 1, b"old").unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_snapshot BEFORE INSERT ON projection_snapshot
             BEGIN SELECT RAISE(ABORT, 'snapshot write failed'); END;",
            )
            .unwrap();
        assert!(store.commit_snapshot("ui", 2, b"new").is_err());
        assert_eq!(store.checkpoint("ui").unwrap(), Some(1));
        assert_eq!(store.snapshot("ui").unwrap().unwrap().state, b"old");
        store
            .conn
            .execute_batch("DROP TRIGGER fail_snapshot")
            .unwrap();
        store.commit_snapshot("ui", 2, b"new").unwrap();
    }

    #[test]
    fn wal_reader_keeps_a_consistent_view_while_another_connection_commits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut writer = StateStore::open(&path).unwrap();
        let reader = StateStore::open(&path).unwrap();
        writer.commit_snapshot("ui", 1, b"old").unwrap();

        let read_tx = reader.conn.unchecked_transaction().unwrap();
        assert_eq!(reader.checkpoint("ui").unwrap(), Some(1));
        writer.commit_snapshot("ui", 2, b"new").unwrap();
        assert_eq!(reader.snapshot("ui").unwrap().unwrap().state, b"old");
        assert_eq!(reader.checkpoint("ui").unwrap(), Some(1));
        read_tx.commit().unwrap();
        assert_eq!(reader.snapshot("ui").unwrap().unwrap().state, b"new");
        assert_eq!(reader.checkpoint("ui").unwrap(), Some(2));
    }

    #[test]
    fn checkpoints_never_wrap_when_stored_or_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = StateStore::open(dir.path().join("state.db")).unwrap();
        let max = i64::MAX as u64;
        store.commit_snapshot("ui", max, b"last").unwrap();
        assert!(store.advance_checkpoint("ui", max + 1).is_err());
        assert!(store.commit_snapshot("ui", u64::MAX, b"overflow").is_err());
        assert_eq!(store.checkpoint("ui").unwrap(), Some(max));

        // A previous release could write a negative INTEGER via an unchecked u64 cast.
        store
            .conn
            .execute("UPDATE projection_checkpoint SET journal_seq = -1", [])
            .unwrap();
        assert!(store.checkpoint("ui").is_err());
    }
}
