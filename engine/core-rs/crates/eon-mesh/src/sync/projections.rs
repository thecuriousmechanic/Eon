use std::collections::{BTreeMap, BTreeSet};

use eon_primitives::{EonError, EonResult};
use loro::{JsonMapOp, LoroValue};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{MAX_PROTECTED_RECORD_BYTES, Replica, loro_err};

// Each record, not the whole projection, is one Loro LWW register. Version the roots
// because changing a postcard enum's discriminants would reinterpret existing history.
const CONTEXT: &str = "context.entries.v2";
const ENTITIES: &str = "knowledge.entities.v2";
const RELATIONS: &str = "knowledge.relations.v2";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Speaker {
    User,
    Assistant,
}

/// Local input, deliberately not serializable. Only `ContextRecord` goes into Loro.
pub enum ContextEntry {
    Utterance { speaker: Speaker, text: String },
    Clipboard { text: String },
    FocusedApp { application: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextRecord {
    Utterance { speaker: Speaker },
    Clipboard,
    FocusedApp { application: String },
}

impl From<&ContextEntry> for ContextRecord {
    fn from(entry: &ContextEntry) -> Self {
        match entry {
            ContextEntry::Utterance { speaker, .. } => Self::Utterance {
                speaker: speaker.clone(),
            },
            ContextEntry::Clipboard { .. } => Self::Clipboard,
            ContextEntry::FocusedApp { application } => Self::FocusedApp {
                application: application.clone(),
            },
        }
    }
}

pub type WorkingContext = BTreeMap<String, ContextRecord>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub kind: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Relation {
    pub source: String,
    pub predicate: String,
    pub target: String,
}

impl Relation {
    fn key(&self) -> EonResult<String> {
        // Length-delimited fields: an embedded separator cannot alias another triple.
        let bytes = postcard::to_allocvec(self).map_err(schema_err)?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeGraph {
    pub entities: BTreeMap<String, Entity>,
    pub relations: BTreeSet<Relation>,
}

fn schema_err(reason: impl std::fmt::Display) -> EonError {
    EonError::ValidationError(format!("mesh projection: {reason}"))
}

fn check_local_record_size(key: &str, value_bytes: usize) -> EonResult<()> {
    if key.len().saturating_add(value_bytes) > MAX_PROTECTED_RECORD_BYTES {
        return Err(schema_err("protected key + encoded value exceeds the local record limit"));
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(value: &LoroValue) -> EonResult<T> {
    let LoroValue::Binary(bytes) = value else {
        return Err(schema_err("expected binary record"));
    };
    let (record, rest) = postcard::take_from_bytes(bytes).map_err(schema_err)?;
    if !rest.is_empty() {
        return Err(schema_err("trailing record bytes"));
    }
    Ok(record)
}

pub(super) fn allowed(root: &str, op: &JsonMapOp) -> bool {
    match (root, op) {
        (CONTEXT | ENTITIES | RELATIONS, JsonMapOp::Delete { .. }) => true,
        (CONTEXT, JsonMapOp::Insert { value, .. }) => decode::<ContextRecord>(value).is_ok(),
        (ENTITIES, JsonMapOp::Insert { value, .. }) => decode::<Entity>(value).is_ok(),
        (RELATIONS, JsonMapOp::Insert { key, value }) => decode::<Relation>(value)
            .and_then(|r| r.key())
            .is_ok_and(|hash| &hash == key),
        _ => false,
    }
}

impl Replica {
    pub fn put_context(&self, id: &str, entry: &ContextEntry) -> EonResult<()> {
        self.put_record(CONTEXT, id, &ContextRecord::from(entry))
    }

    pub fn remove_context(&self, id: &str) -> EonResult<()> {
        self.remove_record(CONTEXT, id)
    }

    pub fn working_context(&self) -> EonResult<WorkingContext> {
        self.records(CONTEXT)
    }

    pub fn put_entity(&self, id: &str, entity: &Entity) -> EonResult<()> {
        self.put_record(ENTITIES, id, entity)
    }

    pub fn remove_entity(&self, id: &str) -> EonResult<()> {
        self.remove_record(ENTITIES, id)
    }

    pub fn put_relation(&self, relation: &Relation) -> EonResult<()> {
        self.put_record(RELATIONS, &relation.key()?, relation)
    }

    pub fn remove_relation(&self, relation: &Relation) -> EonResult<()> {
        self.remove_record(RELATIONS, &relation.key()?)
    }

    /// Relations are visible only while both endpoints exist. Deleting and re-adding
    /// the same entity ID restores its relations unless those were explicitly removed.
    pub fn knowledge_graph(&self) -> EonResult<KnowledgeGraph> {
        let entities = self.records(ENTITIES)?;
        let mut relations = BTreeSet::new();
        for (key, relation) in self.records::<Relation>(RELATIONS)? {
            if key != relation.key()? {
                return Err(schema_err("relation does not match its key"));
            }
            if entities.contains_key(&relation.source) && entities.contains_key(&relation.target) {
                relations.insert(relation);
            }
        }
        Ok(KnowledgeGraph { entities, relations })
    }

    fn put_record(&self, root: &str, id: &str, record: &impl Serialize) -> EonResult<()> {
        check_local_record_size(id, postcard::experimental::serialized_size(record).map_err(schema_err)?)?;
        let bytes = postcard::to_allocvec(record).map_err(schema_err)?;
        self.protected.get_map(root).insert(id, bytes).map_err(loro_err)?;
        self.protected.commit();
        Ok(())
    }

    fn remove_record(&self, root: &str, id: &str) -> EonResult<()> {
        check_local_record_size(id, 0)?;
        self.protected.get_map(root).delete(id).map_err(loro_err)?;
        self.protected.commit();
        Ok(())
    }

    fn records<T: DeserializeOwned>(&self, root: &str) -> EonResult<BTreeMap<String, T>> {
        let map = self.protected.get_map(root).get_value();
        let map = map.as_map().ok_or_else(|| schema_err(root))?;
        map.iter()
            .map(|(key, value)| Ok((key.clone(), decode(value)?)))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn seed_legacy_entity(&self, id: &str, entity: &Entity) {
        let bytes = postcard::to_allocvec(entity).unwrap();
        self.protected.get_map(ENTITIES).insert(id, bytes).unwrap();
        self.protected.commit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crdt_tests_relation_identity_is_not_delimiter_concatenation() {
        let replica = Replica::new(1).unwrap();
        let entity = Entity {
            kind: "document".into(),
            label: "example".into(),
        };
        for id in ["a\0b", "a", "d"] {
            replica.put_entity(id, &entity).unwrap();
        }
        let first = Relation {
            source: "a\0b".into(),
            predicate: "c".into(),
            target: "d".into(),
        };
        let second = Relation {
            source: "a".into(),
            predicate: "b\0c".into(),
            target: "d".into(),
        };
        replica.put_relation(&first).unwrap();
        replica.put_relation(&second).unwrap();
        replica.put_relation(&second).unwrap();
        assert_eq!(
            replica.knowledge_graph().unwrap().relations,
            [first.clone(), second.clone()].into()
        );
        replica.remove_relation(&first).unwrap();
        assert_eq!(replica.knowledge_graph().unwrap().relations, [second].into());
    }

    #[test]
    fn crdt_tests_invalid_imported_record_is_not_a_valid_projection() {
        let sender = Replica::new(1).unwrap();
        let receiver = Replica::new(2).unwrap();
        sender
            .protected
            .get_map(CONTEXT)
            .insert("bad", "not a typed entry")
            .unwrap();
        let delta = sender.paired_export_since(&receiver.paired_version().encode()).unwrap();
        assert!(receiver.paired_apply(&delta).is_err());
        assert!(receiver.working_context().unwrap().is_empty());
        assert!(receiver.knowledge_graph().unwrap().entities.is_empty());
    }
}
