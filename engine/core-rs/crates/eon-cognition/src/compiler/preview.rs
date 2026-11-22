use std::time::{Duration, Instant};

use eon_kernel::{model::Workflow, shadow::Tree};
use eon_primitives::{EonError, EonResult};

use super::{CompiledObjective, ReadRequest, invalid};

pub const CONFIRMATION_WINDOW: Duration = Duration::from_secs(5);

/// A current signal supplied by a trusted UI, never parsed from the intent.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Attention {
    #[default]
    Unverified,
    Verified,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Decision {
    #[default]
    Deny,
    Confirm,
}

pub struct ShadowRead {
    pub request: ReadRequest,
    pub file_len: usize,
    /// Opaque file data, never instructions for additional compiler steps.
    pub bytes: Vec<u8>,
}

/// Only reachable by explicit approval. This is snapshot evidence, not proof
/// of current file contents or permission to dispatch against a live workspace.
pub struct ConfirmedPreview {
    workflow: Workflow,
    reads: Vec<ShadowRead>,
}

impl ConfirmedPreview {
    pub fn into_parts(self) -> (Workflow, Vec<ShadowRead>) {
        (self.workflow, self.reads)
    }
}

pub struct StagedPreview(ConfirmedPreview);

impl CompiledObjective {
    /// Read only the requested slices of the supplied kernel shadow snapshot.
    /// No filesystem fallback, subprocess, mount, merge, or live worker is involved.
    pub fn preview(self, tree: &Tree) -> EonResult<StagedPreview> {
        let mut reads = Vec::with_capacity(self.reads.len());
        for request in self.reads {
            let file = tree
                .get(&request.path)
                .ok_or_else(|| invalid("file not present in the supplied shadow snapshot"))?;
            let slice = usize::try_from(request.offset)
                .ok()
                .and_then(|offset| file.get(offset..))
                .ok_or_else(|| invalid("offset beyond end of shadow file"))?;
            reads.push(ShadowRead {
                file_len: file.len(),
                bytes: slice[..slice.len().min(request.length)].to_vec(),
                request,
            });
        }
        Ok(StagedPreview::new(self.workflow, reads))
    }
}

impl StagedPreview {
    pub(super) fn new(workflow: Workflow, reads: Vec<ShadowRead>) -> Self {
        Self(ConfirmedPreview { workflow, reads })
    }

    pub(super) fn into_confirmed(self) -> ConfirmedPreview {
        self.0
    }

    /// Operation summaries for the UI; neither file bytes nor a submittable workflow.
    pub fn requests(&self) -> impl Iterator<Item = &ReadRequest> {
        self.0.reads.iter().map(|read| &read.request)
    }

    /// Start a single window when the trusted UI actually presents the proposal.
    /// Drop any outstanding window on attention loss or withdrawal of the proposal.
    pub fn present(self, attention: Attention) -> EonResult<Confirmation> {
        Confirmation::start(self, attention)
    }
}

// Neither the window nor the staged plan can be cloned or deserialized.
pub struct Confirmation<T = StagedPreview> {
    preview: T,
    presented_at: Instant,
}

impl<T> Confirmation<T> {
    pub(super) fn start(preview: T, attention: Attention) -> EonResult<Self> {
        Self::start_with(attention, || Ok(preview))
    }

    pub(super) fn start_with(
        attention: Attention,
        freeze: impl FnOnce() -> EonResult<T>,
    ) -> EonResult<Self> {
        verify_attention(attention)?;
        let presented_at = Instant::now();
        Ok(Self {
            preview: freeze()?,
            presented_at,
        })
    }

    /// Withdraw this presentation (e.g. on focus loss). It cannot be resumed.
    pub fn cancel(self) {}

    pub(super) fn approve(self, decision: Decision, attention: Attention) -> EonResult<T> {
        verify_attention(attention)?;
        if decision != Decision::Confirm {
            return Err(EonError::SecurityViolation(
                "preview was not confirmed".into(),
            ));
        }
        if !window_open(self.presented_at, Instant::now()) {
            return Err(EonError::SecurityViolation(
                "confirmation window expired".into(),
            ));
        }
        Ok(self.preview)
    }
}

impl Confirmation {
    /// Dropping or declining the window releases nothing. Both fresh attention and
    /// affirmative consent are required strictly before expiry. No timer activates work.
    pub fn respond(self, decision: Decision, attention: Attention) -> EonResult<ConfirmedPreview> {
        Ok(self.approve(decision, attention)?.into_confirmed())
    }
}

fn window_open(start: Instant, response: Instant) -> bool {
    response
        .checked_duration_since(start)
        .is_some_and(|elapsed| elapsed < CONFIRMATION_WINDOW)
}

fn verify_attention(attention: Attention) -> EonResult<()> {
    if attention != Attention::Verified {
        return Err(EonError::SecurityViolation("attention not verified".into()));
    }
    Ok(())
}

#[cfg(test)]
mod compiler_tests {
    use super::*;

    #[test]
    fn monotonic_confirmation_window_is_half_open() {
        let tick = Duration::from_nanos(1);
        let start = Instant::now() + tick;
        let deadline = start + Duration::from_secs(5);
        assert!(!window_open(start, start - tick));
        assert!(window_open(start, start));
        assert!(window_open(start, deadline - tick));
        assert!(!window_open(start, deadline));
        assert!(!window_open(start, deadline + tick));
    }

    #[test]
    fn freezing_cannot_restart_the_consent_clock() {
        let window = Confirmation::start_with(Attention::Verified, || {
            std::thread::sleep(CONFIRMATION_WINDOW);
            Ok(())
        })
        .unwrap();
        assert!(
            window
                .approve(Decision::Confirm, Attention::Verified)
                .is_err()
        );
    }
}
