//! Read-only recall over a paired graph memory file that another Eon process wrote.
//!
//! Recalled text is untrusted reference material. The UI must render it as plain text and must never
//! treat it as an instruction.

use std::path::Path;

use eon_cognition::memory::{AppraisalMemory, PairedGraphMemory, Provenance};
use eon_primitives::EonResult;
use serde::Serialize;

pub struct Recall {
    memory: PairedGraphMemory,
}

#[derive(Debug, Serialize)]
pub struct HitView {
    pub id: String,
    pub kind: &'static str,
    pub provenance: String,
    pub text: String,
    pub score: f64,
}

impl Recall {
    pub fn open(path: &Path) -> EonResult<Self> {
        let provenance = Provenance::new("eon-web-local-memory")?;
        let memory = PairedGraphMemory::open(path, provenance, AppraisalMemory::MAX_RECORDS)?;
        Ok(Self { memory })
    }

    pub fn recall(&mut self, query: &str, limit: usize) -> EonResult<Vec<HitView>> {
        let hits = self.memory.recall_references(query, limit)?;
        Ok(hits
            .into_iter()
            .map(|hit| HitView {
                id: hit.note.id().to_owned(),
                kind: hit.note.label(),
                provenance: hit.note.provenance().reference().to_owned(),
                text: hit.note.text().to_owned(),
                score: hit.reciprocal_rank_score,
            })
            .collect())
    }
}
