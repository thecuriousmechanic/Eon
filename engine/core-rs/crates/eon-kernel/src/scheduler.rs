//! Single-owner scheduler for one validated workflow and an exclusive resource budget.
//! Task authorization (the submitted workflow's epoch) is independent of the global
//! cognitive-write commit fence. Parallel branches can finish in either order. This runner
//! schedules a static definition; it does not splice live cognitive continuations.
//!
//! [`TaskScheduler::tick`] is the deterministic, explicit-clock interface. Use
//! [`crate::dispatcher::drive`] to wake on lease deadlines even without inbound messages.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eon_primitives::{EonError, EonResult, LeaseId, TaskId};

use crate::events::{EventSink, KernelEvent, transition_recorded};
use crate::fsm::{TaskEvent, TaskState};
use crate::model::workflow::workflow_digest;
use crate::model::{Task, Workflow};
#[cfg(feature = "host")]
use crate::recovery::RecoveryState;
use crate::resource::{Acquisition, ResourceManager};

/// One attempt, created only after its lease is acquired and `Running` acknowledged.
/// Workers must call [`Self::is_valid`] before starting and at effect boundaries.
/// Revocation cannot interrupt an uncooperative provider or undo an external effect.
#[derive(Clone)]
pub struct Dispatch {
    definition: Arc<Workflow>,
    task: TaskId,
    lease: LeaseId,
    granted_at: Instant,
    revoked: Arc<AtomicBool>,
}

impl std::fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A worker logging its permit must not expose the entire workflow's parameters.
        f.debug_struct("Dispatch")
            .field("task", &self.task)
            .field("epoch", &self.epoch())
            .field("lease", &self.lease)
            .finish_non_exhaustive()
    }
}

impl Dispatch {
    pub fn task(&self) -> &TaskId {
        &self.task
    }

    pub fn contract(&self) -> &Task {
        &self.definition.tasks()[&self.task]
    }

    pub fn epoch(&self) -> u64 {
        self.definition.epoch()
    }

    pub fn lease(&self) -> &LeaseId {
        &self.lease
    }

    pub fn is_valid(&self) -> bool {
        self.is_valid_at(Instant::now())
    }

    /// Explicit-clock equivalent for a scheduler driven by a single monotonic clock.
    pub fn is_valid_at(&self, now: Instant) -> bool {
        now >= self.granted_at
            && !self.revoked.load(Ordering::Acquire)
            && !self.remaining(now).is_zero()
    }

    fn remaining(&self, now: Instant) -> Duration {
        Duration::from_millis(self.contract().timeout_ms)
            .saturating_sub(now.saturating_duration_since(self.granted_at))
    }

    fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    /// Definitive failure, with no outstanding side effect requiring reconciliation.
    Failed,
    /// The provider might have applied an effect. Never retried by this scheduler.
    Uncertain,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Countermand {
    Compensated,
    /// A possibly still-running/partially-applied effect makes automatic undo unsafe.
    NeedsReconciliation(Vec<TaskId>),
    /// The undo was attempted once; its outcome must be inspected before undoing ancestors.
    UndoFailed {
        task: TaskId,
        reason: String,
    },
}

pub struct TaskScheduler<S> {
    definition: Arc<Workflow>,
    resources: ResourceManager,
    sink: S,
    states: BTreeMap<TaskId, TaskState>,
    remaining: BTreeMap<TaskId, usize>,
    children: BTreeMap<TaskId, Vec<TaskId>>,
    ready: VecDeque<TaskId>,
    waiters: BTreeMap<u64, TaskId>,
    next_seq: u64,
    active: BTreeMap<TaskId, Dispatch>,
    completed: Vec<TaskId>,
    last_now: Option<Instant>,
    stopped: bool,
}

impl<S: EventSink> TaskScheduler<S> {
    /// Fresh submission only. The caller owns epoch authority and must not submit an
    /// existing workflow twice. Hosts restoring a journal must use `restore`, not this constructor.
    pub fn new(
        definition: Workflow,
        current_epoch: u64,
        resources: ResourceManager,
        sink: S,
    ) -> EonResult<Self> {
        let states = definition
            .tasks()
            .keys()
            .cloned()
            .map(|id| (id, TaskState::Pending))
            .collect();
        let mut scheduler = Self::build(
            definition,
            current_epoch,
            resources,
            sink,
            states,
            Vec::new(),
        )?;
        scheduler.sink.record(KernelEvent::WorkflowSubmitted {
            workflow: scheduler.definition.id().clone(),
            epoch: current_epoch,
            digest: workflow_digest(&scheduler.definition)?,
            tasks: scheduler
                .definition
                .tasks()
                .iter()
                .map(|(id, t)| (id.clone(), t.is_idempotent))
                .collect(),
        })?;
        Ok(scheduler)
    }

    /// Rejoin the original, trusted definition after exclusive journal replay/reconciliation.
    /// Only reconciled Ready tasks can restart. Running state or revoked authority is refused.
    #[cfg(feature = "host")]
    pub fn restore(
        definition: Workflow,
        current_epoch: u64,
        resources: ResourceManager,
        sink: S,
        recovery: &mut RecoveryState,
    ) -> EonResult<Self> {
        let states = recovery.resume_tasks(&definition)?;
        let completed = recovery.completion_order(&definition)?;
        let scheduler = Self::build(
            definition,
            current_epoch,
            resources,
            sink,
            states,
            completed,
        )?;
        recovery.claim_workflow(&scheduler.definition)?;
        Ok(scheduler)
    }

    fn build(
        definition: Workflow,
        current_epoch: u64,
        resources: ResourceManager,
        sink: S,
        states: BTreeMap<TaskId, TaskState>,
        completed: Vec<TaskId>,
    ) -> EonResult<Self> {
        if definition.epoch() != current_epoch {
            return Err(invalid("workflow epoch does not match current authority"));
        }
        if !resources.is_idle() {
            return Err(invalid("scheduler requires an idle resource budget"));
        }
        let mut children: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
        let mut remaining = BTreeMap::new();
        let mut ready = VecDeque::new();
        for (id, task) in definition.tasks() {
            resources.check_capacity(&task.resource_requirements)?;
            let mut count = 0;
            for dep in &task.dependencies {
                children.entry(dep.clone()).or_default().push(id.clone());
                if states[dep] != TaskState::Completed {
                    count += 1;
                }
            }
            match states[id] {
                TaskState::Running | TaskState::Compensating | TaskState::Compensated => {
                    return Err(invalid(
                        "workflow still requires reconciliation or has rolled back",
                    ));
                }
                TaskState::Pending | TaskState::Ready if count == 0 => ready.push_back(id.clone()),
                state if count > 0 && state != TaskState::Pending => {
                    return Err(invalid("task advanced before its dependencies completed"));
                }
                _ => {}
            }
            remaining.insert(id.clone(), count);
        }
        Ok(Self {
            definition: Arc::new(definition),
            resources,
            sink,
            states,
            remaining,
            children,
            ready,
            waiters: BTreeMap::new(),
            next_seq: 0,
            active: BTreeMap::new(),
            completed,
            last_now: None,
            stopped: false,
        })
    }

    pub fn state(&self, task: &TaskId) -> Option<TaskState> {
        self.states.get(task).copied()
    }

    pub fn resources(&self) -> &ResourceManager {
        &self.resources
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Reap timeouts, service the existing FIFO first, then admit newly dependency-ready
    /// tasks (stable ID order for simultaneous siblings). Each grant is handed out once.
    pub fn tick(&mut self, now: Instant) -> EonResult<Vec<Dispatch>> {
        self.advance_clock(now)?;
        let result = self.dispatch_at(now);
        if result.is_err() {
            // Recording may have succeeded despite a lost acknowledgement. Do not retry
            // a transition or expose any part of this dispatch batch.
            self.stop();
        }
        result
    }

    fn dispatch_at(&mut self, now: Instant) -> EonResult<Vec<Dispatch>> {
        let expired: Vec<_> = self
            .active
            .values()
            .filter(|d| !d.is_valid_at(now))
            .map(|d| d.task.clone())
            .collect();
        for id in expired {
            self.active.remove(&id).expect("active lease").revoke();
            self.transition(&id, TaskEvent::OutcomeUncertain)?;
        }
        let mut issued = Vec::new();
        for grant in self.resources.reap_expired(now) {
            let id = self
                .waiters
                .remove(&grant.seq)
                .expect("only this scheduler can queue");
            issued.push(self.start(id, grant.lease, now)?);
        }
        while let Some(id) = self.ready.pop_front() {
            if self.states[&id] == TaskState::Pending {
                self.transition(&id, TaskEvent::DependenciesMet)?;
            }
            let task = &self.definition.tasks()[&id];
            let seq = self.next_seq;
            self.next_seq = seq
                .checked_add(1)
                .ok_or_else(|| invalid("request sequence exhausted"))?;
            match self.resources.acquire_at(
                0,
                seq,
                task.resource_requirements.clone(),
                Duration::from_millis(task.timeout_ms),
                now,
            )? {
                Acquisition::Queued => {
                    self.waiters.insert(seq, id);
                }
                Acquisition::Granted(lease) => issued.push(self.start(id, lease, now)?),
            }
        }
        Ok(issued)
    }

    fn start(&mut self, task: TaskId, lease: LeaseId, granted_at: Instant) -> EonResult<Dispatch> {
        self.transition(&task, TaskEvent::Start)?;
        let permit = Dispatch {
            definition: self.definition.clone(),
            task: task.clone(),
            lease,
            granted_at,
            revoked: Arc::new(AtomicBool::new(false)),
        };
        self.active.insert(task, permit.clone());
        Ok(permit)
    }

    /// Authenticate and consume a result at most once. Expiration wins at the exact
    /// TTL boundary. Only an acknowledged success can unblock downstream work.
    pub fn finish(&mut self, dispatch: &Dispatch, outcome: Outcome, now: Instant) -> EonResult<()> {
        self.advance_clock(now)?;
        let id = dispatch.task();
        if !self.active.get(id).is_some_and(|active| {
            Arc::ptr_eq(&active.definition, &dispatch.definition) && active.lease == dispatch.lease
        }) {
            return Err(invalid("stale, foreign or already consumed dispatch"));
        }
        let expired = !dispatch.is_valid_at(now);
        let event = match (expired, outcome) {
            (true, _) | (_, Outcome::Uncertain) => TaskEvent::OutcomeUncertain,
            (_, Outcome::Succeeded) => TaskEvent::Succeed,
            (_, Outcome::Failed) => TaskEvent::Fail,
        };
        self.active.remove(id).expect("matched above").revoke();
        self.resources.relinquish(dispatch.lease());
        if let Err(err) = self.transition(id, event) {
            self.stop();
            return Err(err);
        }
        if expired {
            return Err(EonError::ResourceLeaseTimeout {
                lease_id: dispatch.lease.to_string(),
            });
        }
        if event == TaskEvent::Succeed {
            self.completed.push(id.clone());
            for child in self.children.get(id).into_iter().flatten() {
                let count = self.remaining.get_mut(child).expect("validated child");
                *count -= 1;
                if *count == 0 {
                    self.ready.push_back(child.clone());
                }
            }
        }
        Ok(())
    }

    /// Record the interrupt before touching any compensation. Revoke all worker permits,
    /// release the budget, then undo known successes in reverse durable completion order.
    /// An uncertain running effect is a barrier: speculative rollback could race a worker.
    pub fn countermand(
        &mut self,
        epoch: u64,
        mut undo: impl FnMut(&Task) -> EonResult<()>,
    ) -> EonResult<Countermand> {
        if self.stopped || epoch != self.definition.epoch() {
            return Err(invalid("inactive or mismatched countermand epoch"));
        }
        // Interrupt workers immediately. Even a failed/lost invalidation acknowledgement
        // cannot safely reauthorize work, and must never trigger speculative compensation.
        self.stop();
        self.sink.record(KernelEvent::EpochInvalidated {
            workflow: self.definition.id().clone(),
            epoch,
        })?;
        let running: Vec<_> = self
            .states
            .iter()
            .filter(|(_, s)| **s == TaskState::Running)
            .map(|(id, _)| id.clone())
            .collect();
        for id in running {
            self.transition(&id, TaskEvent::OutcomeUncertain)?;
        }
        let unknown: Vec<_> = self
            .states
            .iter()
            .filter(|(_, s)| **s == TaskState::UncertainFailure)
            .map(|(id, _)| id.clone())
            .collect();
        if !unknown.is_empty() {
            return Ok(Countermand::NeedsReconciliation(unknown));
        }
        while let Some(id) = self.completed.pop() {
            self.transition(&id, TaskEvent::Rollback)?;
            if let Err(e) = undo(&self.definition.tasks()[&id]) {
                return Ok(Countermand::UndoFailed {
                    task: id,
                    reason: e.to_string(),
                });
            }
            self.transition(&id, TaskEvent::CompensationDone)?;
        }
        Ok(Countermand::Compensated)
    }

    fn transition(&mut self, id: &TaskId, event: TaskEvent) -> EonResult<()> {
        transition_recorded(
            self.states.get_mut(id).expect("validated task"),
            id,
            event,
            &mut self.sink,
        )
    }

    fn advance_clock(&mut self, now: Instant) -> EonResult<()> {
        if self.stopped || self.last_now.is_some_and(|last| now < last) {
            return Err(invalid("scheduler stopped or clock moved backwards"));
        }
        self.last_now = Some(now);
        Ok(())
    }

    pub(crate) fn wake_after(&self, now: Instant) -> Option<Duration> {
        self.active.values().map(|d| d.remaining(now)).min()
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stopped
    }

    pub(crate) fn stop(&mut self) {
        self.stopped = true;
        for permit in self.active.values() {
            permit.revoke();
        }
        self.active.clear();
        self.ready.clear();
        self.waiters.clear();
        self.resources.revoke_all();
    }
}

impl<S> Drop for TaskScheduler<S> {
    fn drop(&mut self) {
        for permit in self.active.values() {
            permit.revoke();
        }
    }
}

fn invalid(message: &str) -> EonError {
    EonError::ValidationError(message.into())
}
