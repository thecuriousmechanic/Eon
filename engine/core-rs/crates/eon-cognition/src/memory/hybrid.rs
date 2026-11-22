use std::{collections::BTreeSet, path::Path};

use eon_primitives::{EonError, EonResult};
use rusqlite::{Connection, TransactionBehavior, params};

use super::{
    TaskDiagnostic, UntrustedNote, WorkingEntry, WorkingForm, WorkingLedger,
    appraisal::{invalid, validate_key},
    graph, reciprocal_rank_fusion,
};

#[cfg(feature = "loro-memory")]
#[path = "loro_graph.rs"]
mod loro_graph;
#[cfg(feature = "loro-memory")]
pub use loro_graph::PairedGraphMemory;

/// A model/tokenizer revision and its output width. Equal widths alone do not
/// make embeddings from different models comparable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddingSpace {
    revision: String,
    dimensions: usize,
}

impl EmbeddingSpace {
    pub fn new(revision: &str, dimensions: usize) -> EonResult<Self> {
        validate_key(revision, 128)?;
        if !(1..=1024).contains(&dimensions) {
            return Err(invalid("unsupported embedding dimensions"));
        }
        Ok(Self {
            revision: revision.into(),
            dimensions,
        })
    }

    /// Accept actual local encoder output. Do not invent a semantic vector
    /// from keywords when the model is unavailable: lexical-only recall works.
    pub fn quantize(&self, values: &[f32]) -> EonResult<Embedding> {
        if self.dimensions != values.len() || values.iter().any(|v| !v.is_finite()) {
            return Err(invalid("invalid embedding shape or non-finite value"));
        }
        let peak = values.iter().fold(0.0_f32, |max, v| max.max(v.abs()));
        if peak == 0.0 {
            return Err(invalid("zero vector has no cosine similarity"));
        }
        // Per-vector scale cancels in cosine. Store signed int8 as byte patterns.
        let bytes = values
            .iter()
            .map(|v| ((v / peak * 127.0).round() as i8) as u8)
            .collect();
        Ok(Embedding {
            space: self.clone(),
            bytes,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Embedding {
    space: EmbeddingSpace,
    bytes: Vec<u8>,
}

fn cosine(query: &[u8], vector: &[u8], query_norm: f64) -> EonResult<f64> {
    if query.len() != vector.len() {
        return Err(invalid("corrupt stored embedding dimensions"));
    }
    let (mut dot, mut norm) = (0_i32, 0_i32);
    for (&a, &b) in query.iter().zip(vector) {
        let (a, b) = (i32::from(a as i8), i32::from(b as i8));
        dot += a * b;
        norm += b * b;
    }
    if norm == 0 {
        return Err(invalid("corrupt stored zero embedding"));
    }
    Ok(f64::from(dot) / (query_norm * f64::from(norm).sqrt()))
}

fn literal_fts_query(text: &str) -> EonResult<String> {
    if text.len() > 1024 {
        return Err(invalid("memory query exceeds 1024 bytes"));
    }
    let mut terms = BTreeSet::new();
    for word in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        terms.insert(format!("\"{}\"", word.to_lowercase()));
        if terms.len() > 32 {
            return Err(invalid("memory query exceeds 32 distinct terms"));
        }
    }
    Ok(terms.into_iter().collect::<Vec<_>>().join(" OR "))
}

pub(super) fn database(error: rusqlite::Error) -> EonError {
    EonError::OperationFailed {
        operation: "memory projection".into(),
        reason: error.to_string(),
    }
}

/// Text and provenance are both untrusted. A high score is relevance, not
/// confidence in truth, and is never a capability or a permission.
#[derive(Clone, Debug, PartialEq)]
pub struct RecallHit {
    pub note: UntrustedNote,
    pub reciprocal_rank_score: f64,
}

/// Bounded local read projection. The FTS5 index and stored int8 vectors
/// change in the same transaction. The working ledger is deliberately transient.
pub struct AppraisalMemory {
    db: Connection,
    space: EmbeddingSpace,
    capacity: usize,
    working: WorkingLedger,
}

impl AppraisalMemory {
    pub const MAX_RECORDS: usize = 2048;
    pub const MAX_RESULTS: usize = 64;

    pub fn open(path: impl AsRef<Path>, space: EmbeddingSpace, capacity: usize) -> EonResult<Self> {
        if !(1..=Self::MAX_RECORDS).contains(&capacity) {
            return Err(invalid("unsupported memory projection capacity"));
        }
        let mut db = Connection::open(path).map_err(database)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA trusted_schema=OFF;
             PRAGMA foreign_keys=ON;",
        )
        .map_err(database)?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database)?;
        tx.execute_batch(include_str!("schema.sql"))
            .map_err(database)?;
        tx.execute(
            "INSERT INTO memory_v1_config(singleton,model,dims,capacity) VALUES(1,?1,?2,?3)
             ON CONFLICT(singleton) DO NOTHING",
            params![space.revision, space.dimensions, capacity],
        )
        .map_err(database)?;
        let matches: bool = tx.query_row(
            "SELECT model=?1 AND dims=?2 AND capacity=?3 FROM memory_v1_config WHERE singleton=1",
            params![space.revision, space.dimensions, capacity], |r| r.get(0),
        ).map_err(database)?;
        if !matches {
            return Err(invalid(
                "memory projection uses a different model, width or capacity",
            ));
        }
        tx.commit().map_err(database)?;
        Ok(Self {
            db,
            space,
            capacity,
            working: WorkingLedger::default(),
        })
    }

    /// Add evidence or correct the note with the same id. `None` clears any
    /// previous vector so a text correction cannot retain stale semantic hits.
    /// A full index refuses new IDs; it never silently forgets durable evidence.
    pub fn appraise(
        &mut self,
        now_ms: u64,
        note: &UntrustedNote,
        vector: Option<&Embedding>,
    ) -> EonResult<()> {
        self.persist(now_ms, note, vector)?;
        self.working.record(now_ms, note.clone(), WorkingForm::Full);
        Ok(())
    }

    /// Archive bounded raw output and its embedding. Keep only the caller's
    /// five-line diagnostic in transient context, and only after commit.
    pub fn appraise_diagnostic(
        &mut self,
        now_ms: u64,
        raw: &UntrustedNote,
        vector: Option<&Embedding>,
        diagnostic: &TaskDiagnostic<'_>,
    ) -> EonResult<()> {
        let digest = diagnostic.digest(raw)?;
        self.persist(now_ms, raw, vector)?;
        self.working.record(now_ms, digest, WorkingForm::TaskDigest);
        Ok(())
    }

    fn persist(
        &mut self,
        now_ms: u64,
        note: &UntrustedNote,
        vector: Option<&Embedding>,
    ) -> EonResult<()> {
        self.working.check_time(now_ms)?;
        check_space(&self.space, vector)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database)?;
        let full: bool = tx
            .query_row(
                "SELECT (SELECT count(*) FROM memory_v1_notes) >= ?1
                AND NOT EXISTS (SELECT 1 FROM memory_v1_notes WHERE id=?2)",
                params![self.capacity, note.id()],
                |r| r.get(0),
            )
            .map_err(database)?;
        if full {
            return Err(invalid("memory projection at capacity"));
        }
        tx.execute(
            "INSERT INTO memory_v1_notes(id,source,body,vector) VALUES(?1,?2,?3,?4)
             ON CONFLICT(id) DO UPDATE SET source=excluded.source, body=excluded.body, vector=excluded.vector",
            params![note.id(), note.provenance().reference(), note.text(), vector.map(|v| &v.bytes)],
        ).map_err(database)?;
        tx.execute("DELETE FROM memory_v1_relations WHERE id=?1", [note.id()])
            .map_err(database)?;
        tx.execute("DELETE FROM memory_v1_entities WHERE id=?1", [note.id()])
            .map_err(database)?;
        graph::insert_kind(&tx, note)?;
        tx.commit().map_err(database)?;
        Ok(())
    }

    /// Replace the projection from a source snapshot in one transaction. Each
    /// record must carry original evidence supplied by its source; a mesh peer
    /// ID alone is not evidence provenance. No partial rebuild is visible.
    pub fn replace_snapshot<'a>(
        &mut self,
        records: impl IntoIterator<Item = (&'a UntrustedNote, Option<&'a Embedding>)>,
    ) -> EonResult<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database)?;
        tx.execute("DELETE FROM memory_v1_notes", [])
            .map_err(database)?;
        {
            let mut insert = tx
                .prepare("INSERT INTO memory_v1_notes(id,source,body,vector) VALUES(?1,?2,?3,?4)")
                .map_err(database)?;
            for (i, (note, vector)) in records.into_iter().enumerate() {
                if i == self.capacity {
                    return Err(invalid("memory snapshot exceeds capacity"));
                }
                check_space(&self.space, vector)?;
                insert
                    .execute(params![
                        note.id(),
                        note.provenance().reference(),
                        note.text(),
                        vector.map(|v| &v.bytes)
                    ])
                    .map_err(database)?;
                graph::insert_kind(&tx, note)?;
            }
        }
        tx.commit().map_err(database)?;
        self.working = WorkingLedger::default();
        Ok(())
    }

    /// Forget a note and any incident graph edges, including their text/vectors.
    /// Logical forgetting in this projection, not secure disk erasure or a
    /// tombstone in the authoritative source (which must also be corrected).
    pub fn forget(&mut self, id: &str) -> EonResult<bool> {
        validate_key(id, 128)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database)?;
        let edges = graph::edges(&tx, id, self.capacity)?;
        {
            let mut del = tx
                .prepare_cached("DELETE FROM memory_v1_notes WHERE id=?1")
                .map_err(database)?;
            for edge in &edges {
                del.execute([edge]).map_err(database)?;
            }
        }
        let n = tx
            .execute("DELETE FROM memory_v1_notes WHERE id=?1", [id])
            .map_err(database)?;
        tx.commit().map_err(database)?;
        for edge in edges {
            self.working.forget(&edge);
        }
        self.working.forget(id);
        Ok(n != 0)
    }

    /// Deterministic one-hop graph lookup; edge provenance is independent of
    /// endpoint provenance. Predicates, including permission claims, are data.
    pub fn related_references(
        &mut self,
        entity_id: &str,
        limit: usize,
    ) -> EonResult<Vec<UntrustedNote>> {
        validate_key(entity_id, 128)?;
        if limit > Self::MAX_RESULTS {
            return Err(invalid("memory result limit exceeded"));
        }
        let tx = self.db.transaction().map_err(database)?;
        let notes = graph::edges(&tx, entity_id, limit)?
            .iter()
            .map(|id| graph::fetch(&tx, id))
            .collect::<EonResult<_>>()?;
        tx.commit().map_err(database)?;
        Ok(notes)
    }

    pub fn working_references(
        &mut self,
        now_ms: u64,
    ) -> EonResult<impl Iterator<Item = &WorkingEntry>> {
        self.working.references(now_ms)
    }

    /// Explicit reference-note retrieval only. Empty evidence returns no hits.
    /// Fuse entire bounded BM25 and positive-cosine rankings, not just their
    /// top-k, so a good cross-channel match can outrank either channel's leader.
    pub fn recall_references(
        &mut self,
        text: &str,
        vector: Option<&Embedding>,
        limit: usize,
    ) -> EonResult<Vec<RecallHit>> {
        let query = literal_fts_query(text)?;
        self.recall_ranked(query, vector, limit)
    }

    fn recall_ranked(
        &mut self,
        query: String,
        vector: Option<&Embedding>,
        limit: usize,
    ) -> EonResult<Vec<RecallHit>> {
        check_space(&self.space, vector)?;
        if limit > Self::MAX_RESULTS {
            return Err(invalid("memory result limit exceeded"));
        }
        if limit == 0 || (query.is_empty() && vector.is_none()) {
            return Ok(Vec::new());
        }
        // Lexical ranking, cosine ranking and body fetch must see one revision.
        let tx = self.db.transaction().map_err(database)?;
        let mut rankings = Vec::with_capacity(2);
        if !query.is_empty() {
            let mut stmt = tx.prepare(
                "SELECT n.id FROM memory_v1_fts JOIN memory_v1_notes n ON n.rowid=memory_v1_fts.rowid
                 WHERE memory_v1_fts MATCH ?1 ORDER BY bm25(memory_v1_fts), n.id LIMIT ?2",
            ).map_err(database)?;
            let ids = stmt
                .query_map(params![query, self.capacity], |r| r.get::<_, String>(0))
                .map_err(database)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(database)?;
            rankings.push(ids);
        }
        if let Some(vector) = vector {
            let query_norm = f64::from(
                vector
                    .bytes
                    .iter()
                    .map(|b| i32::from(*b as i8).pow(2))
                    .sum::<i32>(),
            )
            .sqrt();
            let mut stmt = tx.prepare(
                "SELECT id,vector FROM memory_v1_notes WHERE vector IS NOT NULL ORDER BY id LIMIT ?1",
            ).map_err(database)?;
            let mut rows = stmt.query([self.capacity]).map_err(database)?;
            let mut matches = Vec::new();
            while let Some(row) = rows.next().map_err(database)? {
                let blob = row
                    .get_ref(1)
                    .map_err(database)?
                    .as_blob()
                    .map_err(|_| invalid("corrupt stored embedding"))?;
                let score = cosine(&vector.bytes, blob, query_norm)?;
                if score > 0.0 {
                    matches.push((row.get::<_, String>(0).map_err(database)?, score));
                }
            }
            matches.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            rankings.push(matches.into_iter().map(|(id, _)| id).collect());
        }
        let mut result = Vec::new();
        for (id, reciprocal_rank_score) in reciprocal_rank_fusion(&rankings).into_iter().take(limit)
        {
            result.push(RecallHit {
                note: graph::fetch(&tx, &id)?,
                reciprocal_rank_score,
            });
        }
        tx.commit().map_err(database)?;
        Ok(result)
    }
}

fn check_space(space: &EmbeddingSpace, vector: Option<&Embedding>) -> EonResult<()> {
    if vector.is_some_and(|v| &v.space != space) {
        return Err(invalid("embedding model or dimensions do not match index"));
    }
    Ok(())
}
