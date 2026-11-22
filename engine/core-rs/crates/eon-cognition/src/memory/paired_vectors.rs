use eon_primitives::EonResult;
use rusqlite::{TransactionBehavior, params};

use super::{AppraisalMemory, EmbeddingSpace, PairedGraphMemory, RecallHit, UntrustedNote};
#[cfg(feature = "bge-memory")]
use crate::memory::{BgeMicroV2, hybrid::literal_fts_query};
use crate::memory::{
    appraisal::invalid,
    graph,
    hybrid::{Embedding, check_space, database},
};

impl PairedGraphMemory {
    /// Opt in to local vector indexing on this connection. The first binding
    /// pins the encoder/tokenizer revision and width for the file; reopening
    /// with just `open` still permits lexical recall and projection refresh.
    pub fn bind_embedding_space(&mut self, space: EmbeddingSpace) -> EonResult<()> {
        let tx = self
            .index
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_v1_loro_encoder(
                id INTEGER PRIMARY KEY CHECK(id=1), model TEXT NOT NULL, dims INTEGER NOT NULL
             ) STRICT",
        )
        .map_err(database)?;
        tx.execute(
            "INSERT INTO memory_v1_loro_encoder(id,model,dims) VALUES(1,?1,?2)
             ON CONFLICT(id) DO NOTHING",
            params![space.revision, space.dimensions],
        )
        .map_err(database)?;
        let matches: bool = tx
            .query_row(
                "SELECT model=?1 AND dims=?2 FROM memory_v1_loro_encoder WHERE id=1",
                params![space.revision, space.dimensions],
                |r| r.get(0),
            )
            .map_err(database)?;
        if !matches {
            return Err(invalid("paired index bound to a different embedding space"));
        }
        tx.commit().map_err(database)?;
        self.index.space = space;
        Ok(())
    }

    /// A bounded batch of current, visible references for a local encoder.
    /// Encoding takes place outside the read transaction; this does not add
    /// anything to working context or read any other paired document root.
    pub fn pending_embeddings(&mut self, limit: usize) -> EonResult<Vec<UntrustedNote>> {
        if self.index.space.dimensions == 0 {
            return Err(invalid("no local embedding space bound"));
        }
        if limit > AppraisalMemory::MAX_RESULTS {
            return Err(invalid("memory result limit exceeded"));
        }
        let tx = self.index.db.transaction().map_err(database)?;
        let notes = {
            let mut select = tx
                .prepare_cached(
                    "SELECT id FROM memory_v1_notes WHERE vector IS NULL ORDER BY id LIMIT ?1",
                )
                .map_err(database)?;
            let mut rows = select.query([limit]).map_err(database)?;
            let mut notes = Vec::new();
            while let Some(row) = rows.next().map_err(database)? {
                let id: String = row.get(0).map_err(database)?;
                notes.push(graph::fetch(&tx, &id)?);
            }
            notes
        };
        tx.commit().map_err(database)?;
        Ok(notes)
    }

    /// Attach local encoder output only if the exact observed evidence still
    /// needs a vector. A late result after correction/deletion or a duplicate
    /// completion is a no-op, never an upsert of stale evidence.
    pub fn store_embedding(&mut self, note: &UntrustedNote, vector: &Embedding) -> EonResult<bool> {
        check_space(&self.index.space, Some(vector))?;
        let changed = self
            .index
            .db
            .execute(
                "UPDATE memory_v1_notes SET vector=?1
                 WHERE id=?2 AND source=?3 AND body=?4 AND vector IS NULL",
                params![
                    vector.bytes,
                    note.id(),
                    note.provenance().reference(),
                    note.text()
                ],
            )
            .map_err(database)?;
        Ok(changed != 0)
    }

    /// Fuse local BM25 and positive cosine rankings on the same committed
    /// projection. An empty text query requests vector-only recall.
    pub fn recall_with_embedding(
        &mut self,
        text: &str,
        vector: &Embedding,
        limit: usize,
    ) -> EonResult<Vec<RecallHit>> {
        self.index.recall_references(text, Some(vector), limit)
    }
}

#[cfg(feature = "bge-memory")]
impl PairedGraphMemory {
    fn use_bge(&mut self, model: &BgeMicroV2) -> EonResult<()> {
        if self.index.space != *model.space() {
            self.bind_embedding_space(model.space().clone())?;
        }
        Ok(())
    }

    /// Encode at most `limit` pending graph rows, outside any DB transaction.
    /// Returns rows actually installed; racing edits/completions can reduce
    /// this count. Earlier successes remain committed if a later row fails.
    pub fn index_bge(&mut self, model: &mut BgeMicroV2, limit: usize) -> EonResult<usize> {
        if limit > AppraisalMemory::MAX_RESULTS {
            return Err(invalid("memory result limit exceeded"));
        }
        if limit == 0 {
            return Ok(0);
        }
        self.use_bge(model)?;
        let notes = self.pending_embeddings(limit)?;
        let mut stored = 0;
        for note in notes {
            let vector = model.embed(note.text())?;
            stored += usize::from(self.store_embedding(&note, &vector)?);
        }
        Ok(stored)
    }

    /// Encode a local query and fuse its positive cosine hits with BM25 in
    /// the same SQLite snapshot. Neither query nor hits enter working context.
    pub fn recall_bge(
        &mut self,
        model: &mut BgeMicroV2,
        text: &str,
        limit: usize,
    ) -> EonResult<Vec<RecallHit>> {
        let query = literal_fts_query(text)?;
        if limit > AppraisalMemory::MAX_RESULTS {
            return Err(invalid("memory result limit exceeded"));
        }
        if limit == 0 || text.trim().is_empty() {
            return Ok(Vec::new());
        }
        self.use_bge(model)?;
        let vector = model.embed(text)?;
        self.index.recall_ranked(query, Some(&vector), limit)
    }
}
