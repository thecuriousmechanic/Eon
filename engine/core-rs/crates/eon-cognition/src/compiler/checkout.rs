//! Bounded Linux worktrees. Only captured extents, never live workspace files,
//! are used as lowers. Only the chosen frozen directory can leave consent.
use std::ops::Range;

use eon_kernel::model::{Objective, Workflow};
use eon_primitives::{EonResult, TaskId};

use super::candidates::candidate_plans;
use super::snapshot::{Extent, Sealed, capture};
use super::worktree::BranchTree;
use super::{
    Attention, CandidateDecision, CheckoutBacking, CheckoutDirectory, CompiledObjective,
    Confirmation, ReadRequest, ReadWorkspace, ShadowRead, StagedPreview, WorktreeStore, invalid,
};

/// Metadata for the trusted presentation. `id` names this variant, not the
/// underlying read task (two variants can share one task).
pub struct CheckoutChoice {
    pub id: TaskId,
    pub request: ReadRequest,
    /// The proposed fixed-size overwrite in original file coordinates, if any.
    pub overwrite: Option<Range<u64>>,
    pub backing: CheckoutBacking,
}

struct Branch {
    choice: CheckoutChoice,
    file_len: usize,
    captured_len: usize,
    tree: BranchTree,
}

/// Two independently writable alternatives. Only the trusted host supplies
/// edits; neither the objective nor captured file contents are edit instructions.
pub struct PrivateCheckouts {
    branches: [Branch; 2],
    store: WorktreeStore,
    workspace: (u64, u64),
}

/// No branch files, directories or source descriptors survive into the window,
/// even if the UI never responds. Only the trusted scratch parent stays pinned.
pub struct CheckoutPresentation {
    frozen: [Frozen; 2],
    store: WorktreeStore,
    workspace: (u64, u64),
}

struct Frozen {
    choice: CheckoutChoice,
    file_len: usize,
    bytes: Vec<u8>,
}

/// A read-only filesystem export of the selected branch, **not** evidence of
/// the original file, a dispatch approval or a patch to apply to the host.
pub struct SelectedCheckout {
    pub choice: CheckoutChoice,
    pub file_len: usize,
    pub directory: CheckoutDirectory,
}

/// Stage the two alternative read clauses in private directories. Both must
/// pass the same capture checks as read previews before any directory is made.
pub fn checkout_candidates(
    objective: &Objective,
    workspace: &ReadWorkspace,
    store: &WorktreeStore,
) -> EonResult<PrivateCheckouts> {
    stage_candidates(objective, workspace, store).map(|(trees, _)| trees)
}

pub(super) fn stage_candidates(
    objective: &Objective,
    workspace: &ReadWorkspace,
    store: &WorktreeStore,
) -> EonResult<(PrivateCheckouts, [Workflow; 2])> {
    let [left, right] = candidate_plans(objective)?;
    let (left, first) = left.single_read()?;
    let (right, second) = right.single_read()?;
    let (base, extents) = capture(workspace, [&left, &right].into_iter())?;
    let [a, b]: [_; 2] = extents
        .try_into()
        .map_err(|_| invalid("expected two captured alternatives"))?;
    let trees = PrivateCheckouts::new(
        base,
        [(left, a), (right, b)],
        store,
        workspace.root_identity()?,
    )?;
    Ok((trees, [first, second]))
}

impl CompiledObjective {
    pub(super) fn single_read(self) -> EonResult<(ReadRequest, Workflow)> {
        let [read]: [_; 1] = self
            .reads
            .try_into()
            .map_err(|_| invalid("expected a single read for a private checkout"))?;
        Ok((read, self.workflow))
    }
}

impl PrivateCheckouts {
    pub(super) fn new(
        base: Sealed,
        pair: [(ReadRequest, Extent); 2],
        store: &WorktreeStore,
        workspace: (u64, u64),
    ) -> EonResult<Self> {
        store.check(workspace)?;
        let [(a, left), (b, right)] = pair;
        let lower = store.lower(&base.extent(&left.packed)?)?;
        let other = if left.packed == right.packed {
            lower.clone()
        } else {
            store.lower(&base.extent(&right.packed)?)?
        };
        let branch = |request: ReadRequest, extent: Extent, lower| -> EonResult<Branch> {
            let captured_len = extent.packed.len();
            let (tree, backing) = BranchTree::new(store, &request.path, lower, captured_len)?;
            Ok(Branch {
                choice: CheckoutChoice {
                    id: TaskId::new(),
                    request,
                    overwrite: None,
                    backing,
                },
                file_len: extent.file_len,
                captured_len,
                tree,
            })
        };
        Ok(Self {
            branches: [branch(a, left, lower)?, branch(b, right, other)?],
            store: store.clone(),
            workspace,
        })
    }

    pub fn choices(&self) -> impl Iterator<Item = &CheckoutChoice> {
        self.branches.iter().map(|branch| &branch.choice)
    }

    /// Stage at most one nonempty literal overwrite per alternative. Offsets
    /// are absolute within the original file, bounded by the bytes actually
    /// captured (including short reads at EOF), never by the full file length.
    pub fn overwrite(&mut self, id: &TaskId, offset: u64, bytes: &[u8]) -> EonResult<()> {
        let branch = self
            .branches
            .iter_mut()
            .find(|b| &b.choice.id == id)
            .ok_or_else(|| invalid("unknown checkout choice"))?;
        if bytes.is_empty() || branch.choice.overwrite.is_some() {
            return Err(invalid("expected one nonempty overwrite per checkout"));
        }
        let start = offset
            .checked_sub(branch.choice.request.offset)
            .and_then(|n| usize::try_from(n).ok());
        let end = offset.checked_add(bytes.len() as u64);
        let (Some(start), Some(end)) = (start, end) else {
            return Err(invalid("overwrite outside captured range"));
        };
        if start
            .checked_add(bytes.len())
            .is_none_or(|n| n > branch.captured_len)
        {
            return Err(invalid("overwrite outside captured range"));
        }
        branch.tree.overwrite(start, bytes)?;
        branch.choice.overwrite = Some(offset..end);
        Ok(())
    }

    /// Freeze both choices. Neither can be edited, re-presented, or inspected
    /// through the window. Drop or cancel it on countermand or attention loss.
    pub fn present(self, attention: Attention) -> EonResult<Confirmation<CheckoutPresentation>> {
        Confirmation::start_with(attention, || {
            let [a, b] = self.branches.map(|branch| -> EonResult<Frozen> {
                Ok(Frozen {
                    choice: branch.choice,
                    bytes: branch.tree.freeze()?,
                    file_len: branch.file_len,
                })
            });
            Ok(CheckoutPresentation {
                frozen: [a?, b?],
                store: self.store,
                workspace: self.workspace,
            })
        })
    }

    pub(super) fn read_previews(self, workflows: [Workflow; 2]) -> EonResult<[StagedPreview; 2]> {
        let [a, b] = self.branches;
        let [first, second] = workflows;
        let preview = |branch: Branch, workflow| -> EonResult<StagedPreview> {
            let read = ShadowRead {
                request: branch.choice.request,
                file_len: branch.file_len,
                bytes: branch.tree.freeze()?,
            };
            Ok(StagedPreview::new(workflow, vec![read]))
        };
        Ok([preview(a, first)?, preview(b, second)?])
    }
}

impl Confirmation<CheckoutPresentation> {
    /// Consume both alternatives. Only the matching choice can leave the window.
    pub fn select_checkout(
        self,
        decision: CandidateDecision,
        attention: Attention,
    ) -> EonResult<SelectedCheckout> {
        let (pair, id) = self.approve_candidate(decision, attention)?;
        let selected = pair
            .frozen
            .into_iter()
            .find(|b| b.choice.id == id)
            .ok_or_else(|| invalid("selection not in this presentation"))?;
        pair.store.check(pair.workspace)?;
        let directory =
            CheckoutDirectory::export(&pair.store, &selected.choice.request.path, &selected.bytes)?;
        Ok(SelectedCheckout {
            choice: selected.choice,
            file_len: selected.file_len,
            directory,
        })
    }
}
