use std::{collections::BTreeMap, fs, path::Path};

use eon_foundation::journal::{JournalWriter, replay};
use eon_kernel::events::KernelEvent;
use eon_kernel::fsm::{TaskState, TaskState::*};
use eon_kernel::journal_sink::{decode, encode};
use eon_kernel::model::Workflow;
use eon_kernel::recovery::{
    LocalExecutionRecoveryWorker, create_private_snapshot, workflow_digest,
};
use eon_primitives::{TaskId, WorkflowId};

fn transition(task: &TaskId, from: TaskState, to: TaskState) -> KernelEvent {
    KernelEvent::TaskTransition {
        task: task.clone(),
        from,
        to,
    }
}

fn submission(workflow: &WorkflowId, tasks: &[(&TaskId, bool)]) -> KernelEvent {
    KernelEvent::WorkflowSubmitted {
        workflow: workflow.clone(),
        epoch: 1,
        digest: [0; 32],
        tasks: tasks.iter().map(|(id, i)| ((*id).clone(), *i)).collect(),
    }
}

fn append(dir: &Path, events: &[KernelEvent]) {
    let bytes: Vec<_> = events.iter().map(encode).collect();
    JournalWriter::open(dir)
        .unwrap()
        .append_batch(&bytes.iter().map(Vec::as_slice).collect::<Vec<_>>())
        .unwrap();
}

fn events(dir: &Path) -> Vec<KernelEvent> {
    replay(dir)
        .unwrap()
        .iter()
        .map(|f| decode(&f.payload).unwrap())
        .collect()
}

#[test]
fn resume_binds_the_original_full_definition_without_journaling_parameters() {
    use eon_kernel::model::{Task, TaskSpec, TaskType};
    use eon_primitives::ObjectiveId;
    use std::collections::BTreeSet;

    let wf = WorkflowId::new();
    let objective = ObjectiveId::new();
    let (a, b) = (TaskId::new(), TaskId::new());
    let original = TaskSpec {
        id: a.clone(),
        workflow_id: wf.clone(),
        task_type: TaskType::AtomicExecution,
        capability: "notes".into(),
        operation: "read".into(),
        parameters: serde_json::json!({"note": "private notebook"}),
        dependencies: BTreeSet::new(),
        resource_requirements: Vec::new(),
        timeout_ms: 100,
        is_idempotent: true,
    };
    let make = |spec: TaskSpec, depends: bool| {
        let other = TaskSpec {
            id: b.clone(),
            dependencies: if depends {
                BTreeSet::from([a.clone()])
            } else {
                BTreeSet::new()
            },
            ..original.clone()
        };
        Workflow::new(
            wf.clone(),
            objective.clone(),
            42,
            1,
            [
                Task::try_from(spec).unwrap(),
                Task::try_from(other).unwrap(),
            ],
        )
        .unwrap()
    };
    let submitted = make(original.clone(), true);
    let dir = tempfile::tempdir().unwrap();
    let event = KernelEvent::WorkflowSubmitted {
        workflow: wf.clone(),
        epoch: 1,
        digest: workflow_digest(&submitted).unwrap(),
        tasks: BTreeMap::from([(a.clone(), true), (b.clone(), true)]),
    };
    assert!(
        !String::from_utf8(encode(&event))
            .unwrap()
            .contains("private notebook")
    );
    append(dir.path(), &[event]);
    let worker = LocalExecutionRecoveryWorker::boot(dir.path()).unwrap();
    assert!(worker.state().resume_tasks(&submitted).is_ok());
    for replacement in [
        make(
            TaskSpec {
                operation: "send".into(),
                ..original.clone()
            },
            true,
        ),
        make(
            TaskSpec {
                capability: "messaging".into(),
                ..original.clone()
            },
            true,
        ),
        make(
            TaskSpec {
                timeout_ms: 1,
                ..original.clone()
            },
            true,
        ),
        make(
            TaskSpec {
                parameters: serde_json::json!({"note":"other"}),
                ..original.clone()
            },
            true,
        ),
        make(original.clone(), false),
    ] {
        assert!(worker.state().resume_tasks(&replacement).is_err());
    }

    let legacy = tempfile::tempdir().unwrap();
    append(
        legacy.path(),
        &[
            transition(&a, Pending, Ready),
            transition(&a, Ready, Running),
        ],
    );
    let worker = LocalExecutionRecoveryWorker::boot(legacy.path()).unwrap();
    assert_eq!(worker.state().state(&a), Some(UncertainFailure));
    assert!(worker.state().resume_tasks(&submitted).is_err());
}

#[test]
fn every_interrupted_reconciliation_prefix_converges_to_the_same_durable_decisions() {
    let mut ids = [TaskId::new(), TaskId::new(), TaskId::new()];
    ids.sort();
    let [a, b, legacy] = &ids;
    let mut start = vec![submission(&WorkflowId::new(), &[(a, true), (b, false)])];
    for id in &ids {
        start.extend([
            transition(id, Pending, Ready),
            transition(id, Ready, Running),
        ]);
    }
    let decisions = [
        transition(a, Running, UncertainFailure),
        transition(a, UncertainFailure, Ready),
        transition(b, Running, UncertainFailure),
        transition(legacy, Running, UncertainFailure),
    ];
    let expected: Vec<_> = start.iter().chain(&decisions).cloned().collect();
    for cut in 0..=decisions.len() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &start);
        append(dir.path(), &decisions[..cut]);
        for _ in 0..2 {
            let owner = LocalExecutionRecoveryWorker::boot(dir.path()).unwrap();
            assert_eq!(owner.state().state(a), Some(Ready));
            for other in [b, legacy] {
                assert_eq!(owner.state().state(other), Some(UncertainFailure));
            }
            assert_eq!(events(dir.path()), expected);
            assert!(LocalExecutionRecoveryWorker::boot(dir.path()).is_err());
        }
    }
}

#[test]
fn invalidation_and_missing_definitions_never_authorize_retry() {
    let workflow = WorkflowId::new();
    let id = TaskId::new();
    let legacy = TaskId::new();
    let child = TaskId::new();
    let dir = tempfile::tempdir().unwrap();
    append(
        dir.path(),
        &[
            transition(&legacy, Pending, Completed), // Existing shortcut.
            KernelEvent::SubgraphSpliced {
                parent: legacy.clone(),
                added: vec![child.clone()],
            },
            submission(&workflow, &[(&id, true)]),
            transition(&id, Pending, Ready),
            transition(&id, Ready, Running),
            KernelEvent::EpochInvalidated { workflow, epoch: 1 },
            transition(&child, Pending, Ready),
            transition(&child, Ready, Running),
        ],
    );
    let owner = LocalExecutionRecoveryWorker::boot(dir.path()).unwrap();
    assert_eq!(owner.state().state(&legacy), Some(Completed));
    for interrupted in [id, child] {
        assert_eq!(owner.state().state(&interrupted), Some(UncertainFailure));
    }
}

#[test]
fn invalid_histories_abort_before_any_repair_decisions() {
    let workflow = WorkflowId::new();
    let id = TaskId::new();
    let start = [
        submission(&workflow, &[(&id, false)]),
        transition(&id, Pending, Ready),
        transition(&id, Ready, Running),
    ];
    for tail in [
        vec![transition(&id, Ready, Running)], // Wrong "from".
        vec![transition(&id, Running, Ready)], // Illegal edge.
        vec![submission(&WorkflowId::new(), &[(&id, true)])], // Reused identity.
        vec![
            transition(&id, Running, UncertainFailure),
            transition(&id, UncertainFailure, Ready),
        ],
        vec![
            KernelEvent::EpochInvalidated {
                workflow: workflow.clone(),
                epoch: 1,
            },
            transition(&id, Running, Completed),
        ],
    ] {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &start);
        append(dir.path(), &tail);
        let before = replay(dir.path()).unwrap();
        assert!(LocalExecutionRecoveryWorker::boot(dir.path()).is_err());
        assert_eq!(replay(dir.path()).unwrap(), before);
    }
    let dir = tempfile::tempdir().unwrap();
    append(dir.path(), &start);
    JournalWriter::open(dir.path())
        .unwrap()
        .append_batch(&[b"unknown"])
        .unwrap();
    assert!(LocalExecutionRecoveryWorker::boot(dir.path()).is_err());
    assert_eq!(replay(dir.path()).unwrap().len(), start.len() + 1);
}

#[test]
fn interrupted_private_rollback_is_repeatable_and_never_touches_the_published_file() {
    for after_rename in [false, true] {
        let id = TaskId::new();
        let dir = tempfile::tempdir().unwrap();
        append(
            dir.path(),
            &[
                submission(&WorkflowId::new(), &[(&id, false)]),
                transition(&id, Pending, Ready),
                transition(&id, Ready, Running),
                transition(&id, Running, UncertainFailure),
            ],
        );
        let live = dir.path().join("published");
        fs::write(&live, b"concurrent edit").unwrap();
        let active = create_private_snapshot(dir.path(), &id).unwrap();
        fs::write(active.join("edit"), b"unpublished").unwrap();
        let abandoned = active.with_file_name(format!("{id}.abandoned-3"));
        if after_rename {
            fs::rename(&active, &abandoned).unwrap();
        }
        for _ in 0..2 {
            let owner = LocalExecutionRecoveryWorker::boot(dir.path()).unwrap();
            assert_eq!(owner.state().state(&id), Some(UncertainFailure));
            assert!(!active.exists());
            assert_eq!(fs::read(abandoned.join("edit")).unwrap(), b"unpublished");
            assert_eq!(fs::read(&live).unwrap(), b"concurrent edit");
        }
    }
}

#[cfg(unix)]
#[test]
fn unsafe_snapshot_binding_blocks_retry_until_repaired() {
    let id = TaskId::new();
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let staging = dir.path().join("task-staging");
    std::os::unix::fs::symlink(elsewhere.path(), &staging).unwrap();
    append(
        dir.path(),
        &[
            submission(&WorkflowId::new(), &[(&id, true)]),
            transition(&id, Pending, Ready),
            transition(&id, Ready, Running),
        ],
    );
    assert!(LocalExecutionRecoveryWorker::boot(dir.path()).is_err());
    assert_eq!(
        events(dir.path()).last(),
        Some(&transition(&id, Running, UncertainFailure))
    );
    assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    fs::remove_file(staging).unwrap();
    let owner = LocalExecutionRecoveryWorker::boot(dir.path()).unwrap();
    assert_eq!(owner.state().state(&id), Some(Ready));
}
