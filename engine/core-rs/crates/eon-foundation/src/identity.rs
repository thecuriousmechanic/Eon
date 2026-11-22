use std::sync::Arc;

use eon_primitives::{EonError, EonResult, IdentityId};

use crate::permissions::Permissions;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityKind {
    System,
    User,
    EphemeralAgent { parent: IdentityId },
}

/// Trusted bootstrap capability. Keep it with the host; give workers only their issued identity.
/// A new authority cannot issue grants recognized by an existing authority's brokers.
#[derive(Clone, Default)]
pub struct IdentityAuthority(Arc<()>);

impl IdentityAuthority {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn user(&self, grants: Permissions) -> Identity {
        self.issue(IdentityKind::User, grants)
    }

    pub fn system(&self, grants: Permissions) -> Identity {
        self.issue(IdentityKind::System, grants)
    }

    fn issue(&self, kind: IdentityKind, grants: Permissions) -> Identity {
        let id = IdentityId::new();
        Identity {
            issuer: self.0.clone(),
            owner: id.clone(),
            id,
            kind,
            grants,
        }
    }

    pub fn authorize(&self, caller: &Identity, required: Permissions) -> EonResult<()> {
        if Arc::ptr_eq(&self.0, &caller.issuer) && caller.grants.contains(required) {
            Ok(())
        } else {
            Err(EonError::SecurityViolation("permission denied".into()))
        }
    }
}

/// An in-process capability, not a deserializable claim. Public IDs are labels, not authority.
#[derive(Clone, Debug)]
pub struct Identity {
    issuer: Arc<()>,
    id: IdentityId,
    owner: IdentityId,
    kind: IdentityKind,
    grants: Permissions,
}

impl Identity {
    pub fn id(&self) -> &IdentityId {
        &self.id
    }

    pub fn owner(&self) -> &IdentityId {
        &self.owner
    }

    pub fn kind(&self) -> &IdentityKind {
        &self.kind
    }

    pub fn permissions(&self) -> Permissions {
        self.grants
    }

    /// Delegate within this owner's namespace. Neither spawning nor any other grant is implicit.
    pub fn delegate(&self, grants: Permissions) -> EonResult<Self> {
        if !self.grants.contains(Permissions::SPAWN_SUBAGENT | grants) {
            return Err(EonError::SecurityViolation("delegation denied".into()));
        }
        Ok(Self {
            issuer: self.issuer.clone(),
            id: IdentityId::new(),
            owner: self.owner.clone(),
            kind: IdentityKind::EphemeralAgent {
                parent: self.id.clone(),
            },
            grants,
        })
    }
}
