use eon_kernel::model::{Objective, Workflow};
use eon_primitives::{EonError, EonResult, TaskId};

use super::checkout::stage_candidates;
use super::{
    Attention, CompiledObjective, Confirmation, ConfirmedPreview, Decision, PrivateCheckouts,
    ReadRequest, ReadWorkspace, StagedPreview, WorktreeStore, compile_words, intent_words, invalid,
};

/// Two separately validated single-read DAGs, never an implicit best guess.
pub struct CandidatePreview {
    trees: PrivateCheckouts,
    workflows: [Workflow; 2],
}

pub struct CandidatePresentation([StagedPreview; 2]);

/// Two distinct, independently approved read clauses, or `read file A or B`.
/// Read-only DAG previews use the same filesystem branches as editable
/// checkouts, but cannot stage edits. Ordinary `compile` rejects ambiguity.
pub fn preview_candidates(
    objective: &Objective,
    workspace: &ReadWorkspace,
    store: &WorktreeStore,
) -> EonResult<CandidatePreview> {
    let (trees, workflows) = stage_candidates(objective, workspace, store)?;
    Ok(CandidatePreview { trees, workflows })
}

pub(super) fn candidate_plans(objective: &Objective) -> EonResult<[CompiledObjective; 2]> {
    let words = intent_words(&objective.description)?;
    let (left, right) = match words.as_slice() {
        [verb, file, a, or, b]
            if file.eq_ignore_ascii_case("file") && or.eq_ignore_ascii_case("or") =>
        {
            (
                compile_words(objective, &[verb, file, a])?,
                compile_words(objective, &[verb, file, b])?,
            )
        }
        words => {
            let i = words
                .windows(2)
                .position(|w| w[0].eq_ignore_ascii_case("or") && w[1].eq_ignore_ascii_case("read"))
                .ok_or_else(|| invalid("expected two explicit read alternatives"))?;
            (
                compile_words(objective, &words[..i])?,
                compile_words(objective, &words[i + 1..])?,
            )
        }
    };
    match (left.reads(), right.reads()) {
        ([a], [b]) if (&a.path, a.offset, a.length) != (&b.path, b.offset, b.length) => {
            Ok([left, right])
        }
        _ => Err(invalid("expected exactly two distinct single reads")),
    }
}

impl CandidatePreview {
    /// UI summaries and per-presentation selection IDs, not bytes or workflows.
    pub fn requests(&self) -> impl Iterator<Item = &ReadRequest> {
        self.trees.choices().map(|c| &c.request)
    }

    pub fn present(self, attention: Attention) -> EonResult<Confirmation<CandidatePresentation>> {
        Confirmation::start_with(attention, || {
            self.trees
                .read_previews(self.workflows)
                .map(CandidatePresentation)
        })
    }
}

#[derive(Default)]
pub enum CandidateDecision {
    #[default]
    Nack,
    Confirm(TaskId),
}

impl Confirmation<CandidatePresentation> {
    /// Consume both alternatives. A stale/foreign selection, denial, loss of
    /// attention or expiry cannot release even the first candidate.
    pub fn select(
        self,
        decision: CandidateDecision,
        attention: Attention,
    ) -> EonResult<ConfirmedPreview> {
        let (preview, id) = self.approve_candidate(decision, attention)?;
        preview
            .0
            .into_iter()
            .find(|candidate| candidate.requests().any(|r| r.task_id == id))
            .map(StagedPreview::into_confirmed)
            .ok_or_else(|| invalid("selection not in this presentation"))
    }
}

impl<T> Confirmation<T> {
    pub(super) fn approve_candidate(
        self,
        decision: CandidateDecision,
        attention: Attention,
    ) -> EonResult<(T, TaskId)> {
        let CandidateDecision::Confirm(id) = decision else {
            return Err(EonError::SecurityViolation(
                "candidate presentation declined".into(),
            ));
        };
        Ok((self.approve(Decision::Confirm, attention)?, id))
    }
}
