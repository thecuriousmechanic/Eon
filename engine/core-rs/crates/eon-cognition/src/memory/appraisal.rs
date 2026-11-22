use std::collections::VecDeque;

use eon_primitives::{EonError, EonResult};

pub(super) fn invalid(message: &str) -> EonError {
    EonError::ValidationError(message.into())
}

pub(super) fn validate_key(value: &str, max_bytes: usize) -> EonResult<()> {
    if value.len() > max_bytes || value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(invalid("missing, oversized, or invalid memory identifier"));
    }
    Ok(())
}

/// A caller-supplied reference to the original evidence, not an endorsement of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance(String);

impl Provenance {
    pub fn new(reference: &str) -> EonResult<Self> {
        validate_key(reference, 512)?;
        Ok(Self(reference.into()))
    }

    pub fn reference(&self) -> &str {
        &self.0
    }
}

/// The shape of a reference, never a classification of its authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReferenceKind {
    Note,
    Entity {
        kind: String,
    },
    Relation {
        subject: String,
        predicate: String,
        object: String,
    },
}

/// Reference data only. Even notes attributed to a user are not user instructions,
/// system preferences, integrity endorsements, or evidence of any permission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UntrustedNote {
    id: String,
    provenance: Provenance,
    text: String,
    kind: ReferenceKind,
}

impl UntrustedNote {
    pub const LABEL: &'static str = "UNTRUSTED_RECALLED_DATA";
    pub const MAX_TEXT_BYTES: usize = 16 * 1024;

    pub fn new(id: &str, provenance: Provenance, text: &str) -> EonResult<Self> {
        validate_key(id, 128)?;
        if text.len() > Self::MAX_TEXT_BYTES || text.trim().is_empty() || text.contains('\0') {
            return Err(invalid("missing, oversized, or invalid memory text"));
        }
        Ok(Self {
            id: id.into(),
            provenance,
            text: text.into(),
            kind: ReferenceKind::Note,
        })
    }

    pub fn as_entity(mut self, kind: &str) -> EonResult<Self> {
        validate_key(kind, 128)?;
        self.kind = ReferenceKind::Entity { kind: kind.into() };
        Ok(self)
    }

    pub fn as_relation(mut self, subject: &str, predicate: &str, object: &str) -> EonResult<Self> {
        for field in [subject, predicate, object] {
            validate_key(field, 128)?;
        }
        self.kind = ReferenceKind::Relation {
            subject: subject.into(),
            predicate: predicate.into(),
            object: object.into(),
        };
        Ok(self)
    }

    pub fn kind(&self) -> &ReferenceKind {
        &self.kind
    }

    pub fn label(&self) -> &'static str {
        Self::LABEL
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn payload_bytes(&self) -> usize {
        let metadata = match &self.kind {
            ReferenceKind::Note => 0,
            ReferenceKind::Entity { kind } => kind.len(),
            ReferenceKind::Relation {
                subject,
                predicate,
                object,
            } => subject.len() + predicate.len() + object.len(),
        };
        Self::LABEL.len() + self.id.len() + self.provenance.0.len() + self.text.len() + metadata
    }
}

/// Caller-reported fields, not parsed assertions about whether a task succeeded.
/// Missing fields remain explicitly unavailable. Digests do not verify a task.
pub struct TaskDiagnostic<'a> {
    pub exit_code: i32,
    pub duration_ms: u64,
    pub location: Option<(&'a str, u32)>,
    pub signature: Option<&'a str>,
    pub input_sha256: Option<[u8; 32]>,
    pub output_sha256: Option<[u8; 32]>,
}

impl TaskDiagnostic<'_> {
    pub(super) fn digest(&self, raw: &UntrustedNote) -> EonResult<UntrustedNote> {
        fn hex(hash: Option<[u8; 32]>) -> String {
            use std::fmt::Write;
            let Some(bytes) = hash else {
                return "unavailable".into();
            };
            let mut out = String::with_capacity(64);
            for byte in bytes {
                write!(out, "{byte:02x}").expect("writing to a String");
            }
            out
        }
        let location = if let Some((file, line)) = self.location {
            validate_key(file, 256)?;
            format!("{file}:{line}")
        } else {
            "unavailable".into()
        };
        if let Some(signature) = self.signature {
            validate_key(signature, 384)?;
        }
        let body = format!(
            "exit={} duration_ms={}\nlocation={location}\nsignature={}\ninput_sha256={}\noutput_sha256={}",
            self.exit_code,
            self.duration_ms,
            self.signature.unwrap_or("unavailable"),
            hex(self.input_sha256),
            hex(self.output_sha256)
        );
        Ok(UntrustedNote {
            id: raw.id.clone(),
            provenance: raw.provenance.clone(),
            text: body,
            kind: raw.kind.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkingForm {
    Full,
    Excerpt,
    TaskDigest,
}

#[derive(Debug)]
pub struct WorkingEntry {
    observed_ms: u64,
    note: UntrustedNote,
    form: WorkingForm,
}

impl WorkingEntry {
    pub fn note(&self) -> &UntrustedNote {
        &self.note
    }

    pub fn form(&self) -> WorkingForm {
        self.form
    }

    fn compact(&mut self) {
        const EXCERPT_BYTES: usize = 384;
        const GAP: &str = "\n[...]\n";
        let text = &self.note.text;
        if self.form == WorkingForm::Full && text.len() > EXCERPT_BYTES {
            let half = (EXCERPT_BYTES - GAP.len()) / 2;
            let head = text.floor_char_boundary(half);
            let tail = text.ceil_char_boundary(text.len() - half);
            self.note.text = format!("{}{GAP}{}", &text[..head], &text[tail..]);
            self.form = WorkingForm::Excerpt;
        }
    }
}

/// Transient reference context on a caller-supplied monotonic millisecond clock.
/// One byte per token is conservative for byte-fallback tokenizers. This budget
/// covers the data fields, not any prompt framing or other model inputs.
#[derive(Debug, Default)]
pub struct WorkingLedger {
    entries: VecDeque<WorkingEntry>,
    last_ms: u64,
}

impl WorkingLedger {
    pub const PAYLOAD_BUDGET: usize = 4_000;
    pub const WINDOW_MS: u64 = 5 * 60 * 1000;

    pub fn observe(&mut self, now_ms: u64, note: &UntrustedNote) -> EonResult<()> {
        self.check_time(now_ms)?;
        self.record(now_ms, note.clone(), WorkingForm::Full);
        Ok(())
    }

    pub fn observe_diagnostic(
        &mut self,
        now_ms: u64,
        raw: &UntrustedNote,
        diagnostic: &TaskDiagnostic<'_>,
    ) -> EonResult<()> {
        self.check_time(now_ms)?;
        self.record(now_ms, diagnostic.digest(raw)?, WorkingForm::TaskDigest);
        Ok(())
    }

    pub(super) fn check_time(&self, now_ms: u64) -> EonResult<()> {
        if now_ms < self.last_ms {
            return Err(invalid("working context clock moved backwards"));
        }
        Ok(())
    }

    fn expire(&mut self, now_ms: u64) {
        self.last_ms = now_ms;
        while self
            .entries
            .front()
            .is_some_and(|e| now_ms - e.observed_ms >= Self::WINDOW_MS)
        {
            self.entries.pop_front();
        }
    }

    pub(super) fn record(&mut self, now_ms: u64, note: UntrustedNote, form: WorkingForm) {
        self.expire(now_ms);
        self.forget(note.id());
        self.entries.push_back(WorkingEntry {
            observed_ms: now_ms,
            note,
            form,
        });
        let mut size: usize = self.entries.iter().map(|e| e.note.payload_bytes()).sum();
        for entry in &mut self.entries {
            if size <= Self::PAYLOAD_BUDGET {
                break;
            }
            let before = entry.note.payload_bytes();
            entry.compact();
            size -= before - entry.note.payload_bytes();
        }
        while size > Self::PAYLOAD_BUDGET {
            size -= self
                .entries
                .pop_front()
                .expect("nonempty ledger over budget")
                .note
                .payload_bytes();
        }
    }

    pub fn references(&mut self, now_ms: u64) -> EonResult<impl Iterator<Item = &WorkingEntry>> {
        self.check_time(now_ms)?;
        self.expire(now_ms);
        Ok(self.entries.iter())
    }

    pub fn forget(&mut self, id: &str) {
        self.entries.retain(|entry| entry.note.id() != id);
    }
}
