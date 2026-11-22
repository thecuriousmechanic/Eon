//! Read side of `eon-mesh/src/sync/projections.rs`. The two v2 postcard
//! structs serialize as ordered string tuples. Never decode a public string
//! map, working context, history, or arbitrary wire updates as evidence.

use std::{cmp::Ordering, collections::BTreeMap, path::Path};

use eon_primitives::EonResult;
use loro::{Container, Frontiers, Index, LoroDoc, LoroMap, LoroValue, ValueOrContainer};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::{AppraisalMemory, EmbeddingSpace, RecallHit, database, graph, invalid, validate_key};
use crate::memory::{Provenance, UntrustedNote, WorkingLedger};

#[path = "paired_vectors.rs"]
mod paired_vectors;

const ENTITIES: &str = "knowledge.entities.v2";
const RELATIONS: &str = "knowledge.relations.v2";
const FIELD: usize = 128;
const ENTITY_BYTES: usize = FIELD + UntrustedNote::MAX_TEXT_BYTES + 5; // postcard length prefixes
const RELATION_BYTES: usize = 3 * (FIELD + 2);

type Notes = BTreeMap<String, UntrustedNote>;

/// A dedicated, source-bound local index. There is no operation to import
/// updates, export its SQLite data, or turn a reference into an instruction.
pub struct PairedGraphMemory {
    index: AppraisalMemory,
    document: Provenance,
}

impl PairedGraphMemory {
    /// Bounds the total live graph input, including relations with absent endpoints.
    pub const MAX_GRAPH_BYTES: usize = 4 * 1024 * 1024;

    /// `document` is a stable local reference to this specific paired document,
    /// not a Loro writer ID, network credential, or claim about the original author.
    /// Use an app-private directory for the plaintext SQLite file and its WAL.
    /// Projection deletion is logical forgetting, not forensic erasure.
    pub fn open(path: impl AsRef<Path>, document: Provenance, capacity: usize) -> EonResult<Self> {
        validate_key(document.reference(), 192)?;
        // Zero width reserves this file for the paired projector, even when
        // a separate local encoder is bound. Appraisal cannot write its notes.
        let space = EmbeddingSpace {
            revision: format!(
                "paired-graph-v2:{}",
                fingerprint(document.reference().as_bytes())
            ),
            dimensions: 0,
        };
        let index = AppraisalMemory::open(path, space, capacity)?;
        index
            .db
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS memory_v1_loro_checkpoint(
               id INTEGER PRIMARY KEY CHECK(id=1),
               frontier BLOB NOT NULL CHECK(length(frontier) BETWEEN 1 AND 1024)
             ) STRICT",
            )
            .map_err(database)?;
        Ok(Self { index, document })
    }

    /// Refresh from the complete, locally admitted, committed state of the
    /// bound document. The owner must serialize refresh with edits, imports and
    /// checkouts. This is a materializer, **not** a pairing or history validator.
    /// Only winning graph values are read. A malformed record (even an orphan)
    /// rejects the whole refresh; an otherwise valid orphan is hidden as in mesh.
    /// Returns true only if graph rows changed. A bounded causal checkpoint is
    /// committed alongside them so an old backup cannot resurrect deletions.
    pub fn refresh(&mut self, doc: &LoroDoc) -> EonResult<bool> {
        if doc.is_detached() || doc.get_pending_txn_len() != 0 || root(doc, "projection").is_some()
        {
            return Err(invalid("expected current state of a paired document"));
        }
        let frontier = doc.state_frontiers();
        if frontier.len() > 64 {
            return Err(invalid("paired frontier exceeds 64 concurrent tips"));
        }
        let entities = map_root(doc, ENTITIES)?;
        let relations = map_root(doc, RELATIONS)?;
        if entities.is_none()
            && relations.is_none()
            && map_root(doc, "context.entries.v2")?.is_none()
        {
            return Err(invalid("missing paired v2 roots"));
        }
        let count = entities
            .as_ref()
            .map_or(0, LoroMap::len)
            .saturating_add(relations.as_ref().map_or(0, LoroMap::len));
        if count > self.index.capacity {
            return Err(invalid("paired graph exceeds record capacity"));
        }

        let mut notes = Notes::new();
        let mut budget = Budget {
            records: self.index.capacity,
            bytes: Self::MAX_GRAPH_BYTES,
        };
        if let Some(map) = entities {
            read_records(&map, &mut budget, ENTITY_BYTES, |key, bytes| {
                let ((kind, label), rest): ((&str, &str), _) =
                    postcard::take_from_bytes(bytes).map_err(|_| bad_record())?;
                if !rest.is_empty() {
                    return Err(bad_record());
                }
                let id = entity_id(key);
                let evidence = self.evidence(ENTITIES, key, bytes)?;
                let note = UntrustedNote::new(&id, evidence, label)?.as_entity(kind)?;
                notes.insert(id, note);
                Ok(())
            })?;
        }
        if let Some(map) = relations {
            read_records(&map, &mut budget, RELATION_BYTES, |key, bytes| {
                let (triple @ (source, predicate, target), rest): ((&str, &str, &str), _) =
                    postcard::take_from_bytes(bytes).map_err(|_| bad_record())?;
                for field in [source, predicate, target] {
                    validate_key(field, FIELD)?;
                }
                // Mesh keys hash canonical postcard serialization, not arbitrary
                // bytes which happen to decode to the same tuple.
                let mut buf = [0; RELATION_BYTES];
                if !rest.is_empty()
                    || postcard::to_slice(&triple, &mut buf).map_err(|_| bad_record())? != bytes
                    || fingerprint(bytes).as_str() != key
                {
                    return Err(bad_record());
                }
                let subject = entity_id(source);
                let object = entity_id(target);
                if notes.contains_key(&subject) && notes.contains_key(&object) {
                    let id = format!("relation:{key}");
                    let evidence = self.evidence(RELATIONS, key, bytes)?;
                    let body = format!("{source} {predicate} {target}");
                    let note = UntrustedNote::new(&id, evidence, &body)?
                        .as_relation(&subject, predicate, &object)?;
                    notes.insert(id, note);
                }
                Ok(())
            })?;
        }
        if doc.is_detached() || doc.get_pending_txn_len() != 0 || doc.state_frontiers() != frontier
        {
            return Err(invalid("paired document changed during refresh"));
        }
        self.install(notes, doc, &frontier)
    }

    /// Both BM25 hits and adjacent edges are local, explicitly untrusted data.
    pub fn recall_references(&mut self, text: &str, limit: usize) -> EonResult<Vec<RecallHit>> {
        self.index.recall_references(text, None, limit)
    }

    /// Explicitly observe one selected hit in the caller's transient ledger.
    /// Never import context from Loro. Recheck the whole reference against a
    /// single current SQLite snapshot; a deleted/corrected hit is a no-op.
    /// Like other observations, this records evidence as of `now_ms`, not a
    /// subscription to future edits or an endorsement of the note.
    pub fn observe_current(
        &mut self,
        ledger: &mut WorkingLedger,
        now_ms: u64,
        note: &UntrustedNote,
    ) -> EonResult<bool> {
        let tx = self.index.db.transaction().map_err(database)?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM memory_v1_notes WHERE id=?1 AND source=?2)",
                params![note.id(), note.provenance().reference()],
                |r| r.get(0),
            )
            .map_err(database)?;
        let matches = exists && graph::fetch(&tx, note.id())? == *note;
        tx.commit().map_err(database)?;
        if matches {
            ledger.observe(now_ms, note)?;
        }
        Ok(matches)
    }

    /// Look up incident edges by the original mesh entity key.
    pub fn related_references(&mut self, key: &str, limit: usize) -> EonResult<Vec<UntrustedNote>> {
        validate_key(key, FIELD)?;
        self.index.related_references(&entity_id(key), limit)
    }

    fn evidence(&self, root: &str, key: &str, bytes: &[u8]) -> EonResult<Provenance> {
        // Pin the exact observed register value, not just its mutable key.
        // The v2 schema does not attest a user, an upstream journal, or a grant.
        let source = self.document.reference();
        Provenance::new(&format!(
            "loro:{}:{source}/{root}/{key}@{}",
            source.len(),
            fingerprint(bytes)
        ))
    }

    fn install(&mut self, mut next: Notes, doc: &LoroDoc, frontier: &Frontiers) -> EonResult<bool> {
        let tx = self
            .index
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database)?;
        checkpoint(&tx, doc, frontier)?;
        let mut removed = Vec::new();
        {
            let mut select = tx
                .prepare("SELECT id,source FROM memory_v1_notes")
                .map_err(database)?;
            let mut rows = select.query([]).map_err(database)?;
            while let Some(row) = rows.next().map_err(database)? {
                let id: String = row.get(0).map_err(database)?;
                let source: String = row.get(1).map_err(database)?;
                if next
                    .get(&id)
                    .is_some_and(|n| n.provenance().reference() == source)
                {
                    next.remove(&id);
                } else {
                    removed.push(id);
                }
            }
        }
        let changed = !removed.is_empty() || !next.is_empty();
        {
            let mut delete = tx
                .prepare_cached("DELETE FROM memory_v1_notes WHERE id=?1")
                .map_err(database)?;
            for id in removed {
                delete.execute([id]).map_err(database)?;
            }
            let mut insert = tx
                .prepare_cached("INSERT INTO memory_v1_notes(id,source,body) VALUES(?1,?2,?3)")
                .map_err(database)?;
            for note in next.values() {
                insert
                    .execute(params![
                        note.id(),
                        note.provenance().reference(),
                        note.text()
                    ])
                    .map_err(database)?;
                graph::insert_kind(&tx, note)?;
            }
        }
        tx.commit().map_err(database)?;
        Ok(changed)
    }
}

fn checkpoint(tx: &Transaction<'_>, doc: &LoroDoc, frontier: &Frontiers) -> EonResult<()> {
    let encoded = frontier.encode();
    let previous: Option<Vec<u8>> = tx
        .query_row(
            "SELECT frontier FROM memory_v1_loro_checkpoint WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(database)?;
    if let Some(previous) = previous {
        if previous == encoded {
            return Ok(());
        }
        let previous =
            Frontiers::decode(&previous).map_err(|_| invalid("invalid paired checkpoint"))?;
        if doc.cmp_frontiers(frontier, &previous).ok() != Some(Some(Ordering::Greater)) {
            return Err(invalid("stale or unrelated paired document"));
        }
    }
    tx.execute(
        "INSERT INTO memory_v1_loro_checkpoint(id,frontier) VALUES(1,?1)
         ON CONFLICT(id) DO UPDATE SET frontier=excluded.frontier",
        [encoded],
    )
    .map_err(database)?;
    Ok(())
}

fn root(doc: &LoroDoc, name: &str) -> Option<ValueOrContainer> {
    // get_map(name) would *create* a missing root and mask schema mistakes.
    doc.get_by_path(&[Index::Key(name.into())])
}

fn map_root(doc: &LoroDoc, name: &str) -> EonResult<Option<LoroMap>> {
    match root(doc, name) {
        None => Ok(None),
        Some(ValueOrContainer::Container(Container::Map(map))) => Ok(Some(map)),
        _ => Err(invalid("wrong paired root container type")),
    }
}

fn bad_record() -> eon_primitives::EonError {
    invalid("invalid paired graph record")
}

fn fingerprint(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn entity_id(key: &str) -> String {
    format!("entity:{}", fingerprint(key.as_bytes()))
}

struct Budget {
    records: usize,
    bytes: usize,
}

fn read_records(
    map: &LoroMap,
    budget: &mut Budget,
    max_bytes: usize,
    mut read: impl FnMut(&str, &[u8]) -> EonResult<()>,
) -> EonResult<()> {
    for key in map.keys() {
        budget.records = budget
            .records
            .checked_sub(1)
            .ok_or_else(|| invalid("paired graph exceeds record capacity"))?;
        validate_key(key.as_str(), FIELD)?;
        let Some(ValueOrContainer::Value(LoroValue::Binary(bytes))) = map.get(key.as_str()) else {
            return Err(bad_record());
        };
        if bytes.len() > max_bytes {
            return Err(bad_record());
        }
        budget.bytes = budget
            .bytes
            .checked_sub(key.len() + bytes.len())
            .ok_or_else(|| invalid("paired graph exceeds byte budget"))?;
        read(key.as_str(), &bytes)?;
    }
    Ok(())
}
