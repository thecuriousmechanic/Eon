use std::collections::BTreeSet;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eon_kernel::dispatcher::{Command, drive};
use eon_kernel::events::{EventSink, KernelEvent, VecSink};
use eon_kernel::fsm::TaskState;
use eon_kernel::model::{Task, TaskSpec, TaskType, Workflow};
use eon_kernel::resource::{
    CanonicalResourceKey, Demand, Requirement, ResourceManager, ResourceRank,
};
use eon_kernel::scheduler::{Countermand, Outcome, TaskScheduler};
use eon_primitives::{EonError, EonResult, ObjectiveId, TaskId, WorkflowId};

const EPOCH: u64 = 27;

fn cpu() -> CanonicalResourceKey {
    CanonicalResourceKey {
        rank: ResourceRank::CpuCores,
        id: String::new(),
    }
}

fn budget(units: u64) -> ResourceManager {
    let mut rm = ResourceManager::new();
    rm.set_capacity(cpu(), units);
    rm
}

fn task(w: &WorkflowId, units: u64, deps: &[&Task], ttl_ms: u64) -> Task {
    Task::try_from(TaskSpec {
        id: TaskId::new(),
        workflow_id: w.clone(),
        task_type: TaskType::AtomicExecution,
        capability: "local.test@v1".into(),
        operation: "append".into(),
        parameters: serde_json::Value::Null,
        dependencies: deps.iter().map(|t| t.id.clone()).collect(),
        resource_requirements: vec![Requirement {
            key: cpu(),
            demand: Demand::Pool(units),
        }],
        timeout_ms: ttl_ms,
        is_idempotent: false,
    })
    .unwrap()
}

fn workflow(w: &WorkflowId, tasks: &[&Task]) -> Workflow {
    Workflow::new(
        w.clone(),
        ObjectiveId::new(),
        0,
        EPOCH,
        tasks.iter().map(|&t| t.clone()),
    )
    .unwrap()
}

fn scheduler(w: &WorkflowId, tasks: &[&Task], capacity: u64) -> TaskScheduler<VecSink> {
    TaskScheduler::new(
        workflow(w, tasks),
        EPOCH,
        budget(capacity),
        VecSink::default(),
    )
    .unwrap()
}

mod scheduler_tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parallel_diamond_gates_join_rejects_duplicate_and_foreign_results() {
        let w = WorkflowId::new();
        let a = task(&w, 1, &[], 10);
        let b = task(&w, 1, &[&a], 10);
        let c = task(&w, 1, &[&a], 10);
        let d = task(&w, 2, &[&b, &c], 10);
        let now = Instant::now();
        let mut s = scheduler(&w, &[&d, &c, &b, &a], 2);
        let [root] = s.tick(now).unwrap().try_into().unwrap();
        assert_eq!((root.epoch(), root.contract()), (EPOCH, &a));
        let mut foreign = scheduler(&w, &[&a], 2);
        let [other] = foreign.tick(now).unwrap().try_into().unwrap();
        assert!(s.finish(&other, Outcome::Succeeded, now).is_err());
        s.finish(&root, Outcome::Succeeded, now).unwrap();
        assert!(!root.is_valid_at(now));
        let [left, right] = s.tick(now).unwrap().try_into().unwrap();
        assert_eq!((left.task(), right.task()), (&b.id, &c.id));
        s.finish(&right, Outcome::Succeeded, now).unwrap();
        assert!(s.tick(now).unwrap().is_empty());
        assert!(s.finish(&right, Outcome::Succeeded, now).is_err());
        s.finish(&left, Outcome::Succeeded, now).unwrap();
        let [join] = s.tick(now).unwrap().try_into().unwrap();
        assert_eq!(join.task(), &d.id);
        s.finish(&join, Outcome::Succeeded, now).unwrap();
        assert!(s.tick(now).unwrap().is_empty());
        let successes = s
            .sink()
            .events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    KernelEvent::TaskTransition {
                        to: TaskState::Completed,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(successes, 4);
        assert_eq!(s.resources().available(&cpu()), 2);
    }

    #[test]
    fn exact_expiry_services_fifo_and_definitive_failure_releases_capacity() {
        let w = WorkflowId::new();
        let holder = task(&w, 2, &[], 10);
        let head = task(&w, 3, &[], 7);
        let small = task(&w, 1, &[], 10);
        let blocked = task(&w, 1, &[&holder], 10);
        let mut s = scheduler(&w, &[&small, &head, &holder, &blocked], 3);
        let now = Instant::now();
        let expiry = now + Duration::from_millis(10);
        let [first] = s.tick(now).unwrap().try_into().unwrap();
        assert_eq!(first.task(), &holder.id);
        assert_eq!(s.resources().waiting_len(), 2);
        assert_eq!(s.resources().available(&cpu()), 1);
        assert!(s.tick(expiry - Duration::from_nanos(1)).unwrap().is_empty());
        assert!(
            s.tick(now).is_err(),
            "backdating must not resurrect a lease"
        );
        assert!(matches!(
            s.finish(&first, Outcome::Succeeded, expiry),
            Err(EonError::ResourceLeaseTimeout { .. })
        ));
        let [large] = s.tick(expiry).unwrap().try_into().unwrap();
        assert_eq!(large.task(), &head.id);
        assert!(
            large.is_valid_at(expiry),
            "queued TTL begins at grant, not arrival"
        );
        s.finish(&large, Outcome::Failed, expiry).unwrap();
        let [last] = s.tick(expiry).unwrap().try_into().unwrap();
        assert_eq!(last.task(), &small.id);
        s.finish(&last, Outcome::Uncertain, expiry).unwrap();
        assert!(s.tick(expiry).unwrap().is_empty());
        assert_eq!(s.state(&holder.id), Some(TaskState::UncertainFailure));
        assert_eq!(s.state(&head.id), Some(TaskState::Failed));
        assert_eq!(s.state(&small.id), Some(TaskState::UncertainFailure));
        assert_eq!(s.state(&blocked.id), Some(TaskState::Pending));
        assert_eq!(s.resources().available(&cpu()), 3);
    }

    #[test]
    fn impossible_downstream_or_wrong_epoch_refused_at_admission() {
        let w = WorkflowId::new();
        let first = task(&w, 1, &[], 10);
        let child = task(&w, 2, &[&first], 10);
        let plan = workflow(&w, &[&first, &child]);
        assert!(
            TaskScheduler::new(plan.clone(), EPOCH - 1, budget(2), VecSink::default()).is_err()
        );
        assert!(matches!(
            TaskScheduler::new(plan, EPOCH, budget(1), VecSink::default()),
            Err(EonError::ResourceExhausted { .. })
        ));
    }

    #[test]
    fn dispatcher_reaps_idle_worker_without_completion_or_poll_command() {
        let w = WorkflowId::new();
        let abandoned = task(&w, 1, &[], 250);
        let waiting = task(&w, 1, &[], 10_000);
        let mut s = scheduler(&w, &[&waiting, &abandoned], 1);
        let (worker_tx, worker_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let scheduler = &mut s;
            let handle =
                scope.spawn(move || drive(scheduler, &worker_tx, &cmd_rx, |_| unreachable!()));
            let first = worker_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            // Deliberately send nothing. The second grant must be driven by the lease timer.
            let second = worker_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(first.task(), &abandoned.id);
            assert_eq!(second.task(), &waiting.id);
            assert!(!first.is_valid_at(Instant::now()));
            let (reply, result) = mpsc::channel();
            cmd_tx
                .send(Command::Finish {
                    dispatch: second,
                    outcome: Outcome::Succeeded,
                    reply,
                })
                .unwrap();
            result
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            handle.join().unwrap().unwrap();
        });
        assert_eq!(s.state(&abandoned.id), Some(TaskState::UncertainFailure));
        assert_eq!(s.state(&waiting.id), Some(TaskState::Completed));
    }

    struct LostAck {
        events: Vec<KernelEvent>,
        fail: TaskState,
    }

    #[test]
    fn dropped_scheduler_or_disconnected_driver_revokes_worker_copies() {
        let w = WorkflowId::new();
        let a = task(&w, 1, &[], 10_000);
        for disconnect in [true, false] {
            let mut s = scheduler(&w, &[&a], 1);
            let [d] = s.tick(Instant::now()).unwrap().try_into().unwrap();
            assert!(d.is_valid());
            if disconnect {
                let (workers, _worker_rx) = mpsc::channel();
                let (control, commands) = mpsc::channel();
                drop(control);
                assert!(drive(&mut s, &workers, &commands, |_| unreachable!()).is_err());
                assert_eq!(s.resources().available(&cpu()), 1);
                assert!(s.tick(Instant::now()).is_err());
            } else {
                drop(s);
            }
            assert!(!d.is_valid());
        }
    }

    #[test]
    fn dispatcher_never_hands_out_a_grant_expired_during_journal_io() {
        struct SlowAck;
        impl EventSink for SlowAck {
            fn record(&mut self, e: KernelEvent) -> EonResult<()> {
                if matches!(
                    e,
                    KernelEvent::TaskTransition {
                        to: TaskState::Running,
                        ..
                    }
                ) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(())
            }
        }
        let w = WorkflowId::new();
        let a = task(&w, 1, &[], 1);
        let mut s = TaskScheduler::new(workflow(&w, &[&a]), EPOCH, budget(1), SlowAck).unwrap();
        let (workers, worker_rx) = mpsc::channel();
        let (_control, commands) = mpsc::channel();
        drive(&mut s, &workers, &commands, |_| unreachable!()).unwrap();
        assert!(worker_rx.try_recv().is_err());
        assert_eq!(s.state(&a.id), Some(TaskState::UncertainFailure));
        assert_eq!(s.resources().available(&cpu()), 1);
    }

    #[test]
    fn pending_countermand_is_processed_before_dispatch() {
        let w = WorkflowId::new();
        let a = task(&w, 1, &[], 10_000);
        let mut s = scheduler(&w, &[&a], 1);
        let (workers, worker_rx) = mpsc::channel();
        let (control, commands) = mpsc::channel();
        let (reply, result) = mpsc::channel();
        control
            .send(Command::Countermand {
                epoch: EPOCH,
                reply,
            })
            .unwrap();
        drive(&mut s, &workers, &commands, |_| unreachable!()).unwrap();
        assert_eq!(result.recv().unwrap().unwrap(), Countermand::Compensated);
        assert!(worker_rx.try_recv().is_err());
        assert_eq!(s.state(&a.id), Some(TaskState::Pending));
    }

    #[cfg(feature = "host")]
    #[test]
    fn malformed_submission_cannot_downgrade_the_digest_or_override_retry_policy() {
        use eon_kernel::journal_sink::{decode, encode};
        let w = WorkflowId::new();
        let a = task(&w, 1, &[], 10);
        let s = scheduler(&w, &[&a], 1);
        let event = &s.sink().events[0];
        let KernelEvent::WorkflowSubmitted { digest, .. } = event else {
            panic!("missing submission")
        };
        assert_eq!(decode(&encode(event)).unwrap(), *event);
        let hash = blake3::Hash::from_bytes(*digest).to_hex();
        let id = &a.id;
        for line in [
            format!("W\t{w}\t{EPOCH}\t{id}\t1"),
            format!("W\t{w}\t{EPOCH}\t{}\t{id}\t1", "A".repeat(64)),
            format!("W\t{w}\t{EPOCH}\t{hash}\t{id}"),
            format!("W\t{w}\t{EPOCH}\t{hash}\t{id}\t2"),
            format!("W\t{w}\t{EPOCH}\t{hash}\t{id}\t0\t{id}\t1"),
        ] {
            assert!(decode(line.as_bytes()).is_err());
        }
    }
    impl EventSink for LostAck {
        fn record(&mut self, e: KernelEvent) -> EonResult<()> {
            let fail = matches!(e, KernelEvent::TaskTransition { to, .. } if to == self.fail);
            self.events.push(e);
            if fail {
                Err(EonError::InternalError("lost ack".into()))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn ambiguous_ack_revokes_every_lease_and_never_redispatches() {
        let w = WorkflowId::new();
        let a = task(&w, 1, &[], 9);
        let b = task(&w, 1, &[], 9);
        let c = task(&w, 2, &[], 9);
        for fail in [TaskState::Running, TaskState::Completed] {
            let now = Instant::now();
            let mut s = TaskScheduler::new(
                workflow(&w, &[&a, &b, &c]),
                EPOCH,
                budget(2),
                LostAck {
                    events: vec![],
                    fail,
                },
            )
            .unwrap();
            if fail == TaskState::Running {
                assert!(s.tick(now).is_err());
            } else {
                let [left, right] = s.tick(now).unwrap().try_into().unwrap();
                assert!(s.finish(&left, Outcome::Succeeded, now).is_err());
                assert!(!right.is_valid_at(now));
                assert!(s.finish(&left, Outcome::Succeeded, now).is_err());
            }
            assert!(s.tick(now).is_err());
            assert_eq!(s.resources().available(&cpu()), 2);
            assert_eq!(s.resources().waiting_len(), 0);
            assert_eq!(
                s.sink()
                    .events
                    .iter()
                    .filter(|e| matches!(e, KernelEvent::TaskTransition { to, .. } if *to == fail))
                    .count(),
                1
            );
        }
    }

    proptest! {
        #[test]
        fn generated_dags_never_overcommit_or_issue_a_task_twice(
            masks in prop::collection::vec(any::<u16>(), 1..16), cap in 1u64..=4
        ) {
            let w = WorkflowId::new();
            let mut tasks: Vec<Task> = vec![];
            for mask in masks {
                let deps = tasks.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, t)| t).collect::<Vec<_>>();
                tasks.push(task(&w, 1 + u64::from(mask) % cap, &deps, 10));
            }
            let mut s = scheduler(&w, &tasks.iter().collect::<Vec<_>>(), cap);
            let mut done = BTreeSet::new();
            let now = Instant::now();
            while done.len() != tasks.len() {
                let batch = s.tick(now).unwrap();
                prop_assert!(!batch.is_empty());
                let mut held = 0;
                for d in &batch {
                    prop_assert!(d.contract().dependencies.is_subset(&done));
                    if let Demand::Pool(units) = d.contract().resource_requirements[0].demand { held += units; }
                }
                prop_assert_eq!(held + s.resources().available(&cpu()), cap);
                for d in batch.iter().rev() {
                    prop_assert!(done.insert(d.task().clone()));
                    s.finish(d, Outcome::Succeeded, now).unwrap();
                }
            }
            prop_assert!(s.tick(now).unwrap().is_empty());
        }
    }
}

#[cfg(feature = "host")]
mod saga_compensation_tests {
    use super::*;
    use eon_foundation::group_commit::GroupCommitJournal;
    use eon_foundation::journal::replay;
    use eon_kernel::journal_sink::{JournalSink, decode};
    use eon_kernel::recovery::LocalExecutionRecoveryWorker;

    #[test]
    fn durable_invalidation_precedes_reverse_effects_and_blocks_restart() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let w = WorkflowId::new();
        let a = task(&w, 1, &[], 10);
        let b = task(&w, 1, &[], 10);
        let plan = workflow(&w, &[&a, &b]);
        let mut effects = vec![];
        {
            let journal = GroupCommitJournal::spawn(dir.path(), 8).unwrap();
            let mut s =
                TaskScheduler::new(plan.clone(), EPOCH, budget(2), JournalSink::new(&journal))
                    .unwrap();
            let now = Instant::now();
            let [left, right] = s.tick(now).unwrap().try_into().unwrap();
            for d in [&right, &left] {
                std::fs::write(workspace.path().join(d.task().as_str()), b"test effect").unwrap();
                effects.push(d.task().clone());
                s.finish(d, Outcome::Succeeded, now).unwrap();
            }
        }
        // The recovered completion order, not task ID or topological order, controls undo.
        {
            let mut recovery = LocalExecutionRecoveryWorker::boot(dir.path()).unwrap();
            let (journal, state) = recovery.parts();
            let mut s = TaskScheduler::restore(
                plan.clone(),
                EPOCH,
                budget(2),
                JournalSink::new(journal),
                state,
            )
            .unwrap();
            assert!(
                TaskScheduler::restore(
                    plan.clone(),
                    EPOCH,
                    budget(2),
                    JournalSink::new(journal),
                    state
                )
                .is_err()
            );
            assert!(s.countermand(EPOCH + 1, |_| unreachable!()).is_err());
            let outcome = s.countermand(EPOCH, |task| {
                let events: Vec<_> = replay(dir.path()).unwrap().iter().map(|f| decode(&f.payload).unwrap()).collect();
                assert!(events.iter().any(|e| matches!(e, KernelEvent::EpochInvalidated { workflow, epoch: EPOCH } if workflow == &w)));
                assert!(matches!(events.last(), Some(KernelEvent::TaskTransition { task: id, to: TaskState::Compensating, .. }) if id == &task.id));
                assert_eq!(effects.pop(), Some(task.id.clone()));
                std::fs::remove_file(workspace.path().join(task.id.as_str())).unwrap();
                Ok(())
            }).unwrap();
            assert_eq!(outcome, Countermand::Compensated);
            assert!(effects.is_empty());
            assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 0);
            assert!(s.countermand(EPOCH, |_| unreachable!()).is_err());
            assert!(s.tick(Instant::now()).is_err());
        }
        let boot = LocalExecutionRecoveryWorker::boot(dir.path()).unwrap();
        assert_eq!(boot.state().state(&a.id), Some(TaskState::Compensated));
        assert_eq!(boot.state().state(&b.id), Some(TaskState::Compensated));
        assert!(boot.state().resume_tasks(&plan).is_err());
    }

    #[test]
    fn countermand_cancels_waiters_and_fences_inflight_work_without_speculative_undo() {
        let w = WorkflowId::new();
        let a = task(&w, 1, &[], 100);
        let b = task(&w, 1, &[], 100);
        let c = task(&w, 1, &[], 100);
        let mut s = scheduler(&w, &[&a, &b, &c], 1);
        let now = Instant::now();
        let [done] = s.tick(now).unwrap().try_into().unwrap();
        s.finish(&done, Outcome::Succeeded, now).unwrap();
        let [in_flight] = s.tick(now).unwrap().try_into().unwrap();
        assert_eq!(
            s.countermand(EPOCH, |_| unreachable!()).unwrap(),
            Countermand::NeedsReconciliation(vec![b.id.clone()])
        );
        assert!(!in_flight.is_valid_at(now));
        assert!(s.finish(&in_flight, Outcome::Succeeded, now).is_err());
        assert_eq!(s.resources().waiting_len(), 0);
        assert_eq!(s.resources().available(&cpu()), 1);
        assert_eq!(s.state(&a.id), Some(TaskState::Completed));
        assert_eq!(s.state(&c.id), Some(TaskState::Ready));
    }

    #[test]
    fn ambiguous_undo_is_not_repeated_or_marked_compensated() {
        let w = WorkflowId::new();
        let t = task(&w, 1, &[], 100);
        let mut s = scheduler(&w, &[&t], 1);
        let now = Instant::now();
        let [d] = s.tick(now).unwrap().try_into().unwrap();
        s.finish(&d, Outcome::Succeeded, now).unwrap();
        let result = s
            .countermand(EPOCH, |_| Err(EonError::InternalError("unknown".into())))
            .unwrap();
        assert!(matches!(result, Countermand::UndoFailed { task, .. } if task == t.id));
        assert_eq!(s.state(&t.id), Some(TaskState::Compensating));
        assert!(s.countermand(EPOCH, |_| unreachable!()).is_err());
    }

    #[test]
    fn unacknowledged_invalidation_never_runs_undo_or_admits_further_work() {
        struct RejectInvalidation;
        impl EventSink for RejectInvalidation {
            fn record(&mut self, e: KernelEvent) -> EonResult<()> {
                if matches!(e, KernelEvent::EpochInvalidated { .. }) {
                    return Err(EonError::InternalError(
                        "lost invalidation acknowledgement".into(),
                    ));
                }
                Ok(())
            }
        }
        let w = WorkflowId::new();
        let a = task(&w, 1, &[], 10_000);
        let mut s =
            TaskScheduler::new(workflow(&w, &[&a]), EPOCH, budget(1), RejectInvalidation).unwrap();
        let now = Instant::now();
        let [d] = s.tick(now).unwrap().try_into().unwrap();
        s.finish(&d, Outcome::Succeeded, now).unwrap();
        assert!(s.countermand(EPOCH, |_| unreachable!()).is_err());
        assert!(s.tick(now).is_err());
        assert_eq!(s.state(&a.id), Some(TaskState::Completed));
    }
}
