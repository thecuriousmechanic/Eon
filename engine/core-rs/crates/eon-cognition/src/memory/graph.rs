use eon_primitives::EonResult;
use rusqlite::{Connection, params};

use super::{Provenance, ReferenceKind, UntrustedNote, appraisal::invalid, hybrid::database};

pub(super) fn insert_kind(db: &Connection, note: &UntrustedNote) -> EonResult<()> {
    match note.kind() {
        ReferenceKind::Note => (),
        ReferenceKind::Entity { kind } => {
            db.prepare_cached("INSERT INTO memory_v1_entities(id,kind) VALUES(?1,?2)")
                .map_err(database)?
                .execute(params![note.id(), kind])
                .map_err(database)?;
        }
        ReferenceKind::Relation {
            subject,
            predicate,
            object,
        } => {
            db.prepare_cached(
                "INSERT INTO memory_v1_relations(id,subject,predicate,object) VALUES(?1,?2,?3,?4)",
            )
            .map_err(database)?
            .execute(params![note.id(), subject, predicate, object])
            .map_err(database)?;
        }
    }
    Ok(())
}

pub(super) fn edges(db: &Connection, endpoint: &str, limit: usize) -> EonResult<Vec<String>> {
    db.prepare_cached(
        "SELECT id FROM memory_v1_relations WHERE subject=?1 OR object=?1 ORDER BY id LIMIT ?2",
    )
    .map_err(database)?
    .query_map(params![endpoint, limit], |row| row.get(0))
    .map_err(database)?
    .collect::<Result<_, _>>()
    .map_err(database)
}

pub(super) fn fetch(db: &Connection, id: &str) -> EonResult<UntrustedNote> {
    let mut stmt = db
        .prepare_cached(
            "SELECT n.source,n.body,e.kind,r.subject,r.predicate,r.object
         FROM memory_v1_notes n LEFT JOIN memory_v1_entities e ON n.id=e.id
         LEFT JOIN memory_v1_relations r ON n.id=r.id WHERE n.id=?1",
        )
        .map_err(database)?;
    let row = stmt
        .query_one([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(database)?;
    let (source, body, kind, subject, predicate, object) = row;
    let note = UntrustedNote::new(id, Provenance::new(&source)?, &body)?;
    match (kind, subject, predicate, object) {
        (None, None, None, None) => Ok(note),
        (Some(kind), None, None, None) => note.as_entity(&kind),
        (None, Some(s), Some(p), Some(o)) => note.as_relation(&s, &p, &o),
        _ => Err(invalid("inconsistent projected fact type")),
    }
}
