//! The frozen segment came from `TaskScheduler` + `JournalSink` at 1fbf439.
//! The producer fsynced both root outputs, acknowledged only the left root, then
//! exited without running destructors. Do not regenerate the oracle at test time.
//! Kernel journals bind, but deliberately do not contain, the complete workflow.
use std::{collections::BTreeMap, fs, io::Write, path::Path, time::Instant};

use eon_foundation::journal::replay;
use eon_kernel::{
    events::KernelEvent,
    fsm::TaskState::{self, *},
    journal_sink::{JournalSink, decode},
    model::Workflow,
    recovery::LocalExecutionRecoveryWorker,
    resource::ResourceManager,
    scheduler::{Dispatch, Outcome, TaskScheduler},
};
use eon_primitives::TaskId;
use tempfile::{NamedTempFile, TempDir};

const PLAN: &str = include_str!("fixtures/fork-join.json");
const SEGMENT: &str = "seg_00000000000000000001.eonj";
const HEX: &str = include_str!("fixtures/interrupted-v1.eonj.hex");

fn captured_segment() -> Vec<u8> {
    let mut bytes = vec![];
    for line in HEX.lines() {
        assert!(line.len().is_multiple_of(2));
        for i in (0..line.len()).step_by(2) {
            bytes.push(u8::from_str_radix(&line[i..i + 2], 16).unwrap());
        }
    }
    bytes
}

fn plan() -> Workflow {
    serde_json::from_str(PLAN).unwrap()
}

fn ids() -> [TaskId; 3] {
    [1, 2, 3].map(|n| format!("task_{n:026}").parse().unwrap())
}

struct Store {
    log: TempDir,
    effects: TempDir,
}

impl Store {
    fn new() -> Self {
        Self {
            log: tempfile::tempdir().unwrap(),
            effects: tempfile::tempdir().unwrap(),
        }
    }

    fn at_cut(segment: &[u8]) -> Self {
        let this = Self::new();
        let mut file = NamedTempFile::new_in(&this.log).unwrap();
        file.write_all(segment).unwrap();
        file.persist_noclobber(this.log.path().join(SEGMENT))
            .unwrap();
        let [left, right, _] = ids();
        // Captured external state, not derived from the replayed plan.
        fs::write(this.effects.path().join(left.as_str()), b"left\n").unwrap();
        fs::write(this.effects.path().join(right.as_str()), b"right\n").unwrap();
        this
    }

    fn boot(&self) -> LocalExecutionRecoveryWorker {
        LocalExecutionRecoveryWorker::boot(self.log.path()).unwrap()
    }

    fn history(&self) -> Vec<(u64, KernelEvent)> {
        replay(self.log.path())
            .unwrap()
            .into_iter()
            .map(|f| (f.seq_no, decode(&f.payload).unwrap()))
            .collect()
    }

    fn segment(&self) -> Vec<u8> {
        fs::read(self.log.path().join(SEGMENT)).unwrap()
    }

    fn files(&self) -> BTreeMap<TaskId, Vec<u8>> {
        fs::read_dir(self.effects.path())
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let task = entry.file_name().to_str().unwrap().parse().unwrap();
                (task, fs::read(entry.path()).unwrap())
            })
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Effect {
    Created,
    VerifiedExisting,
}

/// This provider only publishes under a typed task ID in a test-owned directory.
/// A lost acknowledgement permits verifying identical bytes, never replacing them.
fn execute(dispatch: &Dispatch, effects: &Path, now: Instant) -> Effect {
    assert!(dispatch.is_valid_at(now));
    let task = dispatch.contract();
    assert_eq!(task.capability, "fixture");
    let expected = match task.operation.as_str() {
        "publish" => task.parameters.as_str().unwrap().as_bytes().to_vec(),
        "concatenate" => task
            .dependencies
            .iter()
            .flat_map(|id| fs::read(effects.join(id.as_str())).unwrap())
            .collect(),
        other => panic!("unexpected fixture operation {other}"),
    };
    let path = effects.join(task.id.as_str());
    let effect = match fs::File::create_new(&path) {
        Ok(mut f) => {
            f.write_all(&expected).unwrap();
            f.sync_all().unwrap();
            Effect::Created
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && task.is_idempotent => {
            Effect::VerifiedExisting
        }
        Err(e) => panic!("unexpected dispatch for {}: {e}", task.id),
    };
    assert_eq!(fs::read(path).unwrap(), expected);
    effect
}

fn restore(boot: &mut LocalExecutionRecoveryWorker) -> TaskScheduler<JournalSink<'_>> {
    let workflow = plan();
    let epoch = workflow.epoch();
    let (writer, recovered) = boot.parts();
    TaskScheduler::restore(
        workflow,
        epoch,
        ResourceManager::new(),
        JournalSink::new(writer),
        recovered,
    )
    .unwrap()
}

fn wave(
    run: &mut TaskScheduler<JournalSink<'_>>,
    store: &Store,
    now: Instant,
    expected: &[(TaskId, Effect)],
) {
    let batch = run.tick(now).unwrap();
    assert_eq!(batch.len(), expected.len());
    for (dispatch, (id, effect)) in batch.iter().zip(expected) {
        assert_eq!(dispatch.task(), id);
        assert_eq!(&execute(dispatch, store.effects.path(), now), effect);
        run.finish(dispatch, Outcome::Succeeded, now).unwrap();
    }
}

fn check_settled(store: &Store, states: [TaskState; 3], order: &[TaskId]) {
    let before = store.segment();
    let mut boot = store.boot();
    check_states(&boot, states);
    assert_eq!(boot.state().completion_order(&plan()).unwrap(), order);
    assert!(restore(&mut boot).tick(Instant::now()).unwrap().is_empty());
    assert_eq!(store.segment(), before, "settled reboot changed journal");
}

fn check_states(boot: &LocalExecutionRecoveryWorker, states: [TaskState; 3]) {
    let expected = ids().into_iter().zip(states).collect();
    assert_eq!(boot.state().resume_tasks(&plan()).unwrap(), expected);
}

fn transition(id: &TaskId, from: TaskState, to: TaskState) -> KernelEvent {
    KernelEvent::TaskTransition {
        task: id.clone(),
        from,
        to,
    }
}

#[test]
fn historical_lost_ack_rejoins_the_uninterrupted_dag_and_file_outcomes() {
    let [left, right, join] = ids();
    let now = Instant::now();
    let control = Store::new();
    {
        let mut boot = control.boot();
        let workflow = plan();
        let epoch = workflow.epoch();
        let sink = JournalSink::new(boot.parts().0);
        let mut run = TaskScheduler::new(workflow, epoch, ResourceManager::new(), sink).unwrap();
        let roots = [
            (left.clone(), Effect::Created),
            (right.clone(), Effect::Created),
        ];
        wave(&mut run, &control, now, &roots);
        wave(&mut run, &control, now, &[(join.clone(), Effect::Created)]);
    }
    check_settled(&control, [Completed; 3], &ids());
    let expected_files = BTreeMap::from([
        (left, b"left\n".to_vec()),
        (right.clone(), b"right\n".to_vec()),
        (join.clone(), b"left\nright\n".to_vec()),
    ]);
    assert_eq!(control.files(), expected_files);

    let segment = captured_segment();
    let uninterrupted = control.history();
    let mut expected = uninterrupted[..6].to_vec();
    expected.extend([
        (7, transition(&right, Running, UncertainFailure)),
        (8, transition(&right, UncertainFailure, Ready)),
        (9, transition(&right, Ready, Running)),
    ]);
    // Apart from reconciling and reacquiring the interrupted root, the events
    // must rejoin the control history. Only the writer's monotonic clock differs.
    expected.extend(
        uninterrupted[6..]
            .iter()
            .map(|(seq, e)| (seq + 3, e.clone())),
    );
    // Two copies of the SAME historical inputs; then a distinct failure window
    // with an identical journal but the right-hand effect not yet published.
    for effect in [
        Effect::VerifiedExisting,
        Effect::VerifiedExisting,
        Effect::Created,
    ] {
        let store = Store::at_cut(&segment);
        if effect == Effect::Created {
            fs::remove_file(store.effects.path().join(right.as_str())).unwrap();
        }
        assert_eq!(store.history(), expected[..6]);
        {
            let mut boot = store.boot();
            check_states(&boot, [Completed, Ready, Pending]);
            assert_eq!(store.history(), expected[..8]);
            let mut run = restore(&mut boot);
            wave(&mut run, &store, now, &[(right.clone(), effect)]);
            wave(&mut run, &store, now, &[(join.clone(), Effect::Created)]);
        }
        check_settled(&store, [Completed; 3], &ids());
        assert!(
            store.segment().starts_with(&segment),
            "prefix was rewritten"
        );
        assert_eq!(store.files(), expected_files);
        assert_eq!(store.history(), expected);
    }
}

#[test]
fn torn_ack_never_reissues_non_idempotent_work_or_unblocks_its_join() {
    let [left, right, _] = ids();
    let mut segment = captured_segment();
    segment.pop().unwrap(); // Last byte of left's completion is not durable.
    let store = Store::at_cut(&segment);
    let before = store.files();
    assert!(replay(store.log.path()).is_err());
    {
        let mut boot = store.boot();
        check_states(&boot, [UncertainFailure, Ready, Pending]);
        let decisions = [
            (6, transition(&left, Running, UncertainFailure)),
            (7, transition(&right, Running, UncertainFailure)),
            (8, transition(&right, UncertainFailure, Ready)),
        ];
        assert_eq!(store.history()[5..], decisions);
        let mut run = restore(&mut boot);
        let retry = [(right.clone(), Effect::VerifiedExisting)];
        wave(&mut run, &store, Instant::now(), &retry);
    }
    check_settled(&store, [UncertainFailure, Completed, Pending], &[right]);
    assert_eq!(store.files(), before);
}

#[test]
fn frozen_submission_rejects_a_different_dag_even_with_identical_topological_order() {
    let original = plan();
    let mut replacement: serde_json::Value = serde_json::from_str(PLAN).unwrap();
    let edges = replacement["tasks"][2]["dependencies"]
        .as_array_mut()
        .unwrap();
    edges.remove(0);
    let replacement: Workflow = serde_json::from_value(replacement).unwrap();
    assert_eq!(
        original.validate_topological_order().unwrap(),
        replacement.validate_topological_order().unwrap()
    );
    let store = Store::at_cut(&captured_segment());
    let mut boot = store.boot();
    let before = store.segment();
    let (writer, state) = boot.parts();
    let sink = JournalSink::new(writer);
    let budget = ResourceManager::new();
    assert!(TaskScheduler::restore(replacement, original.epoch(), budget, sink, state).is_err());
    assert_eq!(store.segment(), before);
    assert!(state.resume_tasks(&original).is_ok());
}
