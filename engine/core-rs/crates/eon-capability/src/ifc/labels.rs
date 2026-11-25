//! FIDES-style information-flow labels (docs/implementation/01, section 5.1).
//!
//! Integrity is a chain: `Untrusted < Endorsed < Trusted`. Combining values takes the weakest integrity
//! and the strongest confidentiality. Only values that parse into a restricted scalar can be endorsed,
//! and the scalar is validated when it is constructed, so an endorsement cannot skip validation.

use eon_primitives::{EonError, EonResult};

/// Bound on provenance entries kept per label, so repeated joins cannot grow without limit.
pub const MAX_PROVENANCE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntegrityLevel {
    /// Web pages, third-party repos, external UI trees, decompiled binaries, tool outputs.
    Untrusted = 0,
    /// Passed a deterministic schema parser (integer, enum, SHA-256 hex).
    Endorsed = 1,
    /// Authenticated owner input, direct local TTY, signed configuration.
    Trusted = 2,
}

/// Binary authority tag for capability outcomes. Endorsing a scalar does not
/// make its source an authenticated operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CognitiveTaint {
    TrustedOperator,
    UntrustedExternal,
}

impl From<IntegrityLevel> for CognitiveTaint {
    fn from(level: IntegrityLevel) -> Self {
        match level {
            IntegrityLevel::Trusted => Self::TrustedOperator,
            IntegrityLevel::Endorsed | IntegrityLevel::Untrusted => Self::UntrustedExternal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfidentialityLevel {
    Public = 0,
    /// Local source code and build logs.
    Workspace = 1,
    /// Personal messages, clipboard, local documents, system state.
    Restricted = 2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityLabel {
    pub integrity: IntegrityLevel,
    pub confidentiality: ConfidentialityLevel,
    /// Originating task ids or URLs, in first-seen order.
    pub provenance: Vec<String>,
}

/// A value extracted from untrusted input. Each variant is either a restricted scalar that can be
/// endorsed, or a free-form kind that is always refused.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtractedValue {
    Bool(bool),
    Integer(i64),
    /// Must be finite. NaN and infinities are rejected at construction.
    Float(f64),
    /// Chosen from a closed set that the caller defined at compile time.
    FixedVariant(&'static str),
    /// Must be exactly 64 lowercase hexadecimal characters.
    Sha256Hex(String),
    ShellCommand(String),
    FilePath(String),
    Url(String),
    CodePatch(String),
    RecipientAddress(String),
}

impl ExtractedValue {
    pub fn sha256_hex(candidate: &str) -> EonResult<Self> {
        let valid = candidate.len() == 64 && candidate.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self::Sha256Hex(candidate.to_string()))
        } else {
            Err(EonError::ValidationError("expected 64 lowercase hex characters".to_string()))
        }
    }

    pub fn float(value: f64) -> EonResult<Self> {
        if value.is_finite() {
            Ok(Self::Float(value))
        } else {
            Err(EonError::ValidationError("non-finite float is not a valid extraction".to_string()))
        }
    }

    fn is_restricted_scalar(&self) -> bool {
        matches!(
            self,
            Self::Bool(_) | Self::Integer(_) | Self::Float(_) | Self::FixedVariant(_) | Self::Sha256Hex(_)
        )
    }
}

impl SecurityLabel {
    pub fn new(integrity: IntegrityLevel, confidentiality: ConfidentialityLevel, provenance: Vec<String>) -> Self {
        Self {
            integrity,
            confidentiality,
            provenance: dedup_bounded(provenance),
        }
    }

    /// Label for data received from an external source.
    pub fn external(origin: impl Into<String>) -> Self {
        Self::new(IntegrityLevel::Untrusted, ConfidentialityLevel::Public, vec![origin.into()])
    }

    /// Lattice join: weakest integrity, strongest confidentiality, union of provenance.
    pub fn join(&self, other: &Self) -> Self {
        let mut provenance = self.provenance.clone();
        provenance.extend(other.provenance.iter().cloned());
        Self {
            integrity: self.integrity.min(other.integrity),
            confidentiality: self.confidentiality.max(other.confidentiality),
            provenance: dedup_bounded(provenance),
        }
    }

    /// Returns the label for a value extracted from this label's data.
    ///
    /// Restricted scalars raise `Untrusted` to `Endorsed`. Free-form values are refused with an error,
    /// and an existing `Trusted` label is never lowered.
    pub fn endorse(&self, value: &ExtractedValue) -> EonResult<Self> {
        if !value.is_restricted_scalar() {
            return Err(EonError::SecurityViolation(format!(
                "{value:?} is free-form and cannot be endorsed"
            )));
        }
        Ok(Self {
            integrity: self.integrity.max(IntegrityLevel::Endorsed),
            ..self.clone()
        })
    }
}

fn dedup_bounded(entries: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(entries.len().min(MAX_PROVENANCE));
    for entry in entries {
        if out.len() == MAX_PROVENANCE {
            break;
        }
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(i: IntegrityLevel, c: ConfidentialityLevel) -> SecurityLabel {
        SecurityLabel::new(i, c, vec![])
    }

    #[test]
    fn join_takes_weakest_integrity_and_strongest_confidentiality() {
        let web = label(IntegrityLevel::Untrusted, ConfidentialityLevel::Public);
        let owner = label(IntegrityLevel::Trusted, ConfidentialityLevel::Restricted);
        let joined = web.join(&owner);
        assert_eq!(joined.integrity, IntegrityLevel::Untrusted);
        assert_eq!(joined.confidentiality, ConfidentialityLevel::Restricted);
    }

    #[test]
    fn free_form_values_are_never_endorsed() {
        let web = SecurityLabel::external("https://example.test/page");
        for value in [
            ExtractedValue::ShellCommand("rm -rf /".into()),
            ExtractedValue::FilePath("/etc/passwd".into()),
            ExtractedValue::Url("https://evil.test".into()),
            ExtractedValue::CodePatch("--- a".into()),
            ExtractedValue::RecipientAddress("attacker@example.test".into()),
        ] {
            assert!(web.endorse(&value).is_err(), "{value:?} must stay untrusted");
        }
    }

    #[test]
    fn restricted_scalars_are_endorsed_and_keep_provenance() {
        let web = SecurityLabel::external("https://example.test/page");
        let endorsed = web.endorse(&ExtractedValue::Integer(7)).unwrap();
        assert_eq!(endorsed.integrity, IntegrityLevel::Endorsed);
        assert_eq!(endorsed.provenance, web.provenance);
    }

    #[test]
    fn endorsement_never_lowers_an_existing_trust_level() {
        let owner = label(IntegrityLevel::Trusted, ConfidentialityLevel::Public);
        assert_eq!(owner.endorse(&ExtractedValue::Bool(true)).unwrap().integrity, IntegrityLevel::Trusted);
    }

    #[test]
    fn sha256_constructor_rejects_uppercase_and_wrong_length() {
        assert!(ExtractedValue::sha256_hex(&"a".repeat(64)).is_ok());
        assert!(ExtractedValue::sha256_hex(&"A".repeat(64)).is_err());
        assert!(ExtractedValue::sha256_hex(&"a".repeat(63)).is_err());
        assert!(ExtractedValue::sha256_hex(&format!("{}g", "a".repeat(63))).is_err());
    }

    #[test]
    fn float_constructor_rejects_non_finite_values() {
        assert!(ExtractedValue::float(1.5).is_ok());
        assert!(ExtractedValue::float(f64::NAN).is_err());
        assert!(ExtractedValue::float(f64::INFINITY).is_err());
    }

    #[test]
    fn provenance_is_deduplicated_and_bounded() {
        let many: Vec<String> = (0..40).map(|i| format!("src-{i}")).collect();
        let label = SecurityLabel::new(IntegrityLevel::Trusted, ConfidentialityLevel::Public, many);
        assert_eq!(label.provenance.len(), MAX_PROVENANCE);
        let doubled = SecurityLabel::new(IntegrityLevel::Trusted, ConfidentialityLevel::Public, vec!["a".into(), "a".into()]);
        assert_eq!(doubled.provenance, vec!["a".to_string()]);
    }
}
