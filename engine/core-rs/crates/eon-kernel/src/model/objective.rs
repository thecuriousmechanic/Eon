use std::ops::Deref;

use eon_primitives::{EonError, IdentityId, ObjectiveId};
use serde::{Deserialize, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    Critical,
    High,
    Normal,
    Background,
}

/// Input to [`Objective::try_from`]. Timestamps are nanoseconds from the submitting
/// clock's monotonic origin, not wall time or serialized process-local `Instant`s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectiveSpec {
    pub id: ObjectiveId,
    pub owner_id: IdentityId,
    pub description: String,
    pub priority: Priority,
    pub created_at_ns: u64,
    pub deadline_ns: Option<u64>,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "ObjectiveSpec")]
pub struct Objective(ObjectiveSpec);

impl Serialize for Objective {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl TryFrom<ObjectiveSpec> for Objective {
    type Error = EonError;

    fn try_from(spec: ObjectiveSpec) -> Result<Self, Self::Error> {
        if spec.description.trim().is_empty() {
            return Err(EonError::ValidationError(
                "empty objective description".into(),
            ));
        }
        if spec.deadline_ns.is_some_and(|end| end < spec.created_at_ns) {
            return Err(EonError::ValidationError(
                "deadline precedes objective creation".into(),
            ));
        }
        Ok(Self(spec))
    }
}

impl Deref for Objective {
    type Target = ObjectiveSpec;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
