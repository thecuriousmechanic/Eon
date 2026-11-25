//! Exclusive boot replay for a **kernel event** journal. Never infer retry permission from a
//! task's name, a caller's replacement plan, or the absence of a completion record.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use eon_foundation::group_commit::GroupCommitJournal;
use eon_foundation::journal::replay_pinned;
use eon_foundation::owned_dir::OwnedDirectory;
use eon_primitives::{EonError, EonResult, TaskId, WorkflowId};

use crate::events::{EventSink, KernelEvent, transition_recorded};
use crate::fsm::{ALL_EVENTS, TaskEvent, TaskState, next_state};
use crate::journal_sink::{JournalSink, decode};
use crate::model::Workflow;
pub use crate::model::workflow::workflow_digest;

type Submission = (WorkflowId, u64);

fn invalid(reason: impl Into<String>) -> EonError {
    EonError::ValidationError(reason.into())
}

#[derive(Default)]
struct Declaration {
    digest: [u8; 32],
    tasks: usize,
    invalidated: bool,
    spliced: bool,
    claimed: bool,
}

struct Record {
    submission: Option<Submission>,
    idempotent: bool,
    state: TaskState,
    start_seq: u64,
    changed_seq: u64,
}

impl Record {
    fn pending(submission: Option<Submission>, idempotent: bool) -> Self {
        Self {
            submission,
            idempotent,
            state: TaskState::Pending,
            start_seq: 0,
            changed_seq: 0,
        }
    }
}

/// Post-reconciliation state. Leases and resource waiters are process-local: neither is
/// reconstructed. Restore the scheduler with a freshly configured resource manager.
#[derive(Default)]
pub struct RecoveryState {
    declarations: BTreeMap<Submission, Declaration>,
    records: BTreeMap<TaskId, Record>,
}

impl RecoveryState {
    pub fn state(&self, task: &TaskId) -> Option<TaskState> {
        self.records.get(task).map(|r| r.state)
    }

    /// Validates the complete immutable definition against the durable digest. Journal
    /// metadata deliberately does not contain capability parameters or credentials.
    pub fn resume_tasks(&self, workflow: &Workflow) -> EonResult<BTreeMap<TaskId, TaskState>> {
        let key = (workflow.id().clone(), workflow.epoch());
        let d = self
            .declarations
            .get(&key)
            .ok_or_else(|| invalid("no durable submission for workflow and epoch"))?;
        if d.invalidated
            || d.spliced
            || d.tasks != workflow.tasks().len()
            || d.digest != workflow_digest(workflow)?
        {
            return Err(invalid("fenced, changed or incomplete workflow"));
        }
        workflow
            .tasks()
            .iter()
            .map(|(id, task)| {
                let r = self
                    .records
                    .get(id)
                    .filter(|r| {
                        r.submission.as_ref() == Some(&key) && r.idempotent == task.is_idempotent
                    })
                    .ok_or_else(|| invalid(format!("submission mismatch for {id}")))?;
                Ok((id.clone(), r.state))
            })
            .collect()
    }

    /// Still-completed tasks in durable completion order, for reverse-order compensation.
    pub fn completion_order(&self, workflow: &Workflow) -> EonResult<Vec<TaskId>> {
        let mut completed: Vec<_> = self
            .resume_tasks(workflow)?
            .into_iter()
            .filter(|(_, s)| *s == TaskState::Completed)
            .map(|(id, _)| (self.records[&id].changed_seq, id))
            .collect();
        completed.sort_unstable();
        Ok(completed.into_iter().map(|(_, id)| id).collect())
    }

    /// Consume the right to restore this submission from this snapshot. The restored
    /// scheduler may advance immediately; a second scheduler would see stale progress.
    pub(crate) fn claim_workflow(&mut self, workflow: &Workflow) -> EonResult<()> {
        let declaration = self
            .declarations
            .get_mut(&(workflow.id().clone(), workflow.epoch()))
            .ok_or_else(|| invalid("no durable submission to claim"))?;
        if std::mem::replace(&mut declaration.claimed, true) {
            return Err(invalid("workflow already restored from this boot snapshot"));
        }
        Ok(())
    }

    fn apply(&mut self, seq: u64, event: KernelEvent) -> EonResult<()> {
        match event {
            KernelEvent::WorkflowSubmitted {
                workflow,
                epoch,
                digest,
                tasks,
            } => {
                let key = (workflow, epoch);
                if self.declarations.contains_key(&key)
                    || tasks.keys().any(|id| self.records.contains_key(id))
                {
                    // In particular, a late declaration cannot turn legacy work into a safe retry.
                    return Err(invalid("duplicate submission or reused task identity"));
                }
                self.declarations.insert(
                    key.clone(),
                    Declaration {
                        tasks: tasks.len(),
                        digest,
                        ..Default::default()
                    },
                );
                for (id, idempotent) in tasks {
                    self.records
                        .insert(id, Record::pending(Some(key.clone()), idempotent));
                }
            }
            KernelEvent::EpochInvalidated { workflow, epoch } => {
                let d = self
                    .declarations
                    .get_mut(&(workflow, epoch))
                    .ok_or_else(|| invalid("invalidated undeclared epoch"))?;
                d.invalidated = true;
            }
            KernelEvent::TaskTransition { task, from, to } => {
                let r = self
                    .records
                    .entry(task.clone())
                    .or_insert_with(|| Record::pending(None, false));
                // complete_recorded on the legacy graph wrote Pending -> Completed directly.
                let legacy = r.submission.is_none()
                    && (from, to) == (TaskState::Pending, TaskState::Completed);
                if r.state != from
                    || !(legacy || ALL_EVENTS.iter().any(|e| next_state(from, *e) == Ok(to)))
                {
                    return Err(invalid(format!("inconsistent transition for {task}")));
                }
                let declaration = r.submission.as_ref().and_then(|k| self.declarations.get(k));
                if ((from, to) == (TaskState::UncertainFailure, TaskState::Ready)
                    && !can_retry(r, declaration))
                    || (declaration.is_some_and(|d| d.invalidated)
                        && matches!(
                            to,
                            TaskState::Ready | TaskState::Running | TaskState::Completed
                        ))
                {
                    return Err(invalid(format!("unauthorized transition for {task}")));
                }
                r.state = to;
                r.changed_seq = seq;
                if to == TaskState::Running {
                    r.start_seq = seq;
                }
            }
            KernelEvent::SubgraphSpliced { parent, added } => {
                // Legacy splice records have no executable definition or idempotency metadata.
                // Don't resume the original static definition as though it still described the DAG.
                let parent = self
                    .records
                    .entry(parent)
                    .or_insert_with(|| Record::pending(None, false));
                if let Some(key) = &parent.submission {
                    self.declarations
                        .get_mut(key)
                        .expect("declared owner")
                        .spliced = true;
                }
                for id in added {
                    if self
                        .records
                        .insert(id, Record::pending(None, false))
                        .is_some()
                    {
                        return Err(invalid("splice reused a task identity"));
                    }
                }
            }
            KernelEvent::EpochCommitted { .. } => {}
        }
        Ok(())
    }

    fn reconcile(&mut self, dir: &OwnedDirectory, sink: &mut dyn EventSink) -> EonResult<()> {
        // TaskId order is independent of hash seeds, clocks and restart count.
        for (task, r) in &mut self.records {
            if r.state == TaskState::Running {
                transition_recorded(&mut r.state, task, TaskEvent::OutcomeUncertain, sink)?;
            }
            if r.state != TaskState::UncertainFailure {
                continue;
            }
            // A crash after either the uncertainty record or the rename resumes here.
            abandon_private_snapshot(dir, task, r.start_seq)?;
            if can_retry(
                r,
                r.submission.as_ref().and_then(|k| self.declarations.get(k)),
            ) {
                transition_recorded(&mut r.state, task, TaskEvent::RequeueIdempotent, sink)?;
            }
        }
        Ok(())
    }
}

fn can_retry(record: &Record, declaration: Option<&Declaration>) -> bool {
    record.idempotent && declaration.is_some_and(|d| !d.invalidated && !d.spliced)
}

/// Holds exclusive journal ownership continuously, from tail repair through resumed execution.
pub struct LocalExecutionRecoveryWorker {
    journal: GroupCommitJournal,
    state: RecoveryState,
}

impl LocalExecutionRecoveryWorker {
    pub fn boot(dir: impl AsRef<Path>) -> EonResult<Self> {
        let dir = dir.as_ref();
        let journal = GroupCommitJournal::spawn(dir, 32)?;
        let mut state = RecoveryState::default();
        // No recovery checkpoint yet stores the validated declarations and epoch fences: scan
        // from sequence one, validating the *entire* history before appending any decisions.
        for frame in replay_pinned(journal.directory(), 0)? {
            state
                .apply(frame.seq_no, decode(&frame.payload)?)
                .map_err(|e| invalid(format!("kernel journal sequence {}: {e}", frame.seq_no)))?;
        }
        state.reconcile(journal.directory(), &mut JournalSink::new(&journal))?;
        Ok(Self { journal, state })
    }

    pub fn state(&self) -> &RecoveryState {
        &self.state
    }

    /// Borrow the writer and its matching boot state without releasing the writer lock.
    pub fn parts(&mut self) -> (&GroupCommitJournal, &mut RecoveryState) {
        (&self.journal, &mut self.state)
    }
}

fn io_err(e: std::io::Error) -> EonError {
    EonError::OperationFailed {
        operation: "recovery.private_snapshot".into(),
        reason: e.to_string(),
    }
}

fn directory_exists(path: &Path) -> EonResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(invalid(format!(
            "not a private directory: {}",
            path.display()
        ))),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_err(e)),
    }
}

fn sync_dir(path: &Path) -> EonResult<()> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(io_err)
}

/// Creates an unpublished, task-private working copy directory for the current lease holder.
/// The journal directory must be trusted and exclusively owned by the engine. Recovery rolls
/// these copies back by detaching the whole directory, never by writing into a live user tree.
pub fn create_private_snapshot(journal: &Path, task: &TaskId) -> EonResult<PathBuf> {
    let held = OwnedDirectory::open(journal, false).map_err(io_err)?;
    let mut private = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut private, 0o700);
    let staging = held.path().join("task-staging");
    match private.create(&staging) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists && directory_exists(&staging)? => {}
        Err(e) => return Err(io_err(e)),
    }
    sync_dir(held.path())?;
    let staging = held.child("task-staging").map_err(io_err)?;
    let active = staging.path().join(task.as_str());
    private.create(&active).map_err(io_err)?;
    sync_dir(staging.path())?;
    Ok(journal.join("task-staging").join(task.as_str()))
}

fn abandon_private_snapshot(
    journal: &OwnedDirectory,
    task: &TaskId,
    start_seq: u64,
) -> EonResult<()> {
    let held = match journal.child("task-staging") {
        Ok(dir) => dir,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err(e)),
    };
    let staging = held.path();
    let active = staging.join(task.as_str());
    let abandoned = staging.join(format!("{task}.abandoned-{start_seq}"));
    match (directory_exists(&active)?, directory_exists(&abandoned)?) {
        (false, false) => return Ok(()),
        (true, false) => fs::rename(active, abandoned).map_err(io_err)?,
        (false, true) => {} // Restart after rename but possibly before directory fsync.
        (true, true) => return Err(invalid(format!("conflicting private snapshots for {task}"))),
    }
    sync_dir(staging)
}
