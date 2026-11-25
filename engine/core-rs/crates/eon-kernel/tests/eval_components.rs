//! Portable components of the full evaluation: kernel replay and source-only regressions.
#[path = "eval-harness/defensive_regressions.rs"]
mod defensive_regressions;
#[path = "eval-harness/replay_engine.rs"]
mod replay_engine;

use std::{collections::BTreeMap, fs, time::Instant};

use eon_foundation::journal::replay;
use eon_kernel::{
    fsm::TaskState,
    journal_sink::JournalSink,
    model::{Task, TaskSpec, TaskType, Workflow},
    recovery::LocalExecutionRecoveryWorker,
    resource::ResourceManager,
    scheduler::{Outcome, TaskScheduler},
};
use eon_primitives::{ObjectiveId, TaskId, WorkflowId};

#[test]
fn identical_submissions_and_verified_results_replay_to_the_same_dag_and_outcomes() {
    let wf = WorkflowId::new();
    let ids = [TaskId::new(), TaskId::new(), TaskId::new()];
    let tasks = ids.iter().enumerate().map(|(i, id)| {
        Task::try_from(TaskSpec {
            id: id.clone(),
            workflow_id: wf.clone(),
            task_type: TaskType::AtomicExecution,
            capability: "fixture".into(),
            operation: "write-once".into(),
            parameters: serde_json::Value::Null,
            dependencies: if i == 2 {
                ids[..2].iter().cloned().collect()
            } else {
                Default::default()
            },
            resource_requirements: Vec::new(),
            timeout_ms: 60_000,
            is_idempotent: false,
        })
        .unwrap()
    });
    let plan = Workflow::new(wf.clone(), ObjectiveId::new(), 0, 1, tasks).unwrap();
    let wire = serde_json::to_vec(&plan).unwrap();
    let mut baseline = None;
    for _ in 0..2 {
        let log = tempfile::tempdir().unwrap();
        let effects = tempfile::tempdir().unwrap();
        let plan: Workflow = serde_json::from_slice(&wire).unwrap();
        let expected = plan.validate_topological_order().unwrap();
        {
            let mut boot = LocalExecutionRecoveryWorker::boot(log.path()).unwrap();
            let mut run = TaskScheduler::new(
                plan.clone(),
                plan.epoch(),
                ResourceManager::new(),
                JournalSink::new(boot.parts().0),
            )
            .unwrap();
            // Entries are named by task identity, not scheduler return position. A
            // duplicated dispatch fails at create_dir before it can be acknowledged.
            for expected_wave in [&expected[..2], &expected[2..]] {
                let batch = run.tick(Instant::now()).unwrap();
                let observed: Vec<_> = batch.iter().map(|d| d.task().clone()).collect();
                assert_eq!(observed, expected_wave);
                for d in batch {
                    for dep in &d.contract().dependencies {
                        assert!(effects.path().join(dep.to_string()).is_dir());
                    }
                    fs::create_dir(effects.path().join(d.task().to_string())).unwrap();
                    run.finish(&d, Outcome::Succeeded, Instant::now()).unwrap();
                }
            }
        }
        assert_eq!(fs::read_dir(effects.path()).unwrap().count(), 3);
        let frames = || {
            replay(log.path())
                .unwrap()
                .into_iter()
                .map(|f| (f.seq_no, f.payload))
                .collect::<Vec<_>>()
        };
        let recorded = frames();
        assert!(!recorded.is_empty());
        let states = {
            let mut boot = LocalExecutionRecoveryWorker::boot(log.path()).unwrap();
            assert_eq!(boot.state().completion_order(&plan).unwrap(), expected);
            let states = boot.state().resume_tasks(&plan).unwrap();
            let (journal, state) = boot.parts();
            let mut restored = TaskScheduler::restore(
                plan,
                1,
                ResourceManager::new(),
                JournalSink::new(journal),
                state,
            )
            .unwrap();
            assert!(restored.tick(Instant::now()).unwrap().is_empty());
            states
        };
        assert_eq!(frames(), recorded, "replay appended to a completed history");
        let completed: BTreeMap<_, _> = ids
            .iter()
            .cloned()
            .map(|id| (id, TaskState::Completed))
            .collect();
        assert_eq!(states, completed);
        let result = (expected, recorded, states);
        if let Some(first) = &baseline {
            assert_eq!(first, &result);
        } else {
            baseline = Some(result);
        }
    }
}
