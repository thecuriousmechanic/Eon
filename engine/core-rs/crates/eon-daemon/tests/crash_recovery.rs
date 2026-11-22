//! Kill the engine *after* task four changes a file and *before* it reports an outcome.
//! The file contents and invocation log are independent of the kernel's own journal.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use eon_foundation::journal::replay;
use eon_kernel::fsm::TaskState;
use eon_kernel::journal_sink::JournalSink;
use eon_kernel::model::{Task, TaskSpec, TaskType, Workflow};
use eon_kernel::recovery::create_private_snapshot;
use eon_kernel::resource::{
    CanonicalResourceKey, Demand, Requirement, ResourceManager, ResourceRank,
};
use eon_kernel::scheduler::{Outcome, TaskScheduler};
use eon_primitives::{EonError, LeaseId, TaskId, WorkflowId};

const ROOT: &str = "EON_TEST_CRASH_RECOVERY_DIR";
const PHASE: &str = "EON_TEST_CRASH_RECOVERY_PHASE";
const RETRY: &str = "EON_TEST_CRASH_RECOVERY_RETRY";

fn id(n: usize) -> TaskId {
    format!("task_{n:026}").parse().unwrap()
}

fn cpu() -> CanonicalResourceKey {
    CanonicalResourceKey {
        rank: ResourceRank::CpuCores,
        id: String::new(),
    }
}

fn workflow(safe: bool) -> Workflow {
    let wf: WorkflowId = "wf_00000000000000000000000001".parse().unwrap();
    let tasks = (1..=8).map(|n| {
        let deps: BTreeSet<_> = match n {
            1 => BTreeSet::new(),
            5 => BTreeSet::from([id(3)]), // Independent of the interrupted effect.
            8 => BTreeSet::from([id(4), id(7)]), // Never crosses an uncertain dependency.
            _ => BTreeSet::from([id(n - 1)]),
        };
        Task::try_from(TaskSpec {
            id: id(n),
            workflow_id: wf.clone(),
            task_type: TaskType::AtomicExecution,
            capability: "local.file".into(),
            operation: if n == 4 && safe { "replace" } else { "append" }.into(),
            parameters: Default::default(),
            dependencies: deps,
            resource_requirements: vec![Requirement {
                key: cpu(),
                demand: Demand::Pool(1),
            }],
            timeout_ms: 60_000,
            is_idempotent: n == 4 && safe,
        })
        .unwrap()
    });
    Workflow::new(
        wf.clone(),
        "obj_00000000000000000000000001".parse().unwrap(),
        0,
        1,
        tasks,
    )
    .unwrap()
}

fn append(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

#[test]
#[ignore = "child process for the SIGKILL tests"]
fn execution_child() {
    let root = std::env::var_os(ROOT).unwrap();
    let root = Path::new(&root);
    let initial = std::env::var(PHASE).unwrap() == "initial";
    let safe = std::env::var(RETRY).unwrap() == "true";
    let journal = root.join("journal");
    let mut boot = eon_daemon::boot(&journal).unwrap();
    let (writer, recovered) = boot.parts();
    let mut resources = ResourceManager::new();
    resources.set_capacity(cpu(), 1);
    let wf = workflow(safe);
    let sink = JournalSink::new(writer);
    let mut scheduler = if initial {
        TaskScheduler::new(wf, 1, resources, sink).unwrap()
    } else {
        let stale: LeaseId = fs::read_to_string(root.join("lease"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(matches!(
            resources.release(&stale),
            Err(EonError::ResourceLeaseTimeout { .. })
        ));
        TaskScheduler::restore(wf, 1, resources, sink, recovered).unwrap()
    };
    assert_eq!(scheduler.resources().available(&cpu()), 1);
    assert_eq!(scheduler.resources().waiting_len(), 0);
    loop {
        let batch = scheduler.tick(Instant::now()).unwrap();
        if batch.is_empty() {
            break;
        }
        let [dispatch] = batch.as_slice() else {
            panic!("more than one CPU lease")
        };
        let n = (1..=8).find(|n| id(*n) == *dispatch.task()).unwrap();
        assert!(dispatch.is_valid_at(Instant::now()));
        assert_eq!(scheduler.resources().available(&cpu()), 0);
        append(&root.join("invocations"), &[n as u8]);
        let effect = root.join(format!("effect-{n}"));
        if dispatch.contract().operation == "replace" {
            let mut file = File::create(effect).unwrap();
            file.write_all(b"x").unwrap();
            file.sync_all().unwrap();
        } else {
            append(&effect, b"x");
        }
        if initial && n == 4 {
            // Task 5's stale waiter must be reconstructed, not granted twice or forgotten.
            assert_eq!(scheduler.resources().waiting_len(), 1);
            let staged = create_private_snapshot(&journal, dispatch.task()).unwrap();
            append(&staged.join("edit"), b"unpublished");
            append(&root.join("lease"), dispatch.lease().as_str().as_bytes());
            File::create(root.join("kill-now")).unwrap();
            loop {
                thread::park();
            }
        }
        if !initial {
            assert_ne!(
                dispatch.lease().as_str(),
                fs::read_to_string(root.join("lease")).unwrap()
            );
        }
        scheduler
            .finish(dispatch, Outcome::Succeeded, Instant::now())
            .unwrap();
    }
    assert!(!initial, "initial execution missed task 4");
    assert_eq!(scheduler.resources().available(&cpu()), 1);
    assert_eq!(scheduler.resources().waiting_len(), 0);
}

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Process {
    fn wait_until(&mut self, ready: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready() {
            assert!(
                self.0.try_wait().unwrap().is_none(),
                "child exited before handshake"
            );
            assert!(Instant::now() < deadline, "child missed handshake");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn kill(&mut self) {
        self.0.kill().unwrap();
        assert_eq!(self.0.wait().unwrap().signal(), Some(libc::SIGKILL));
    }
}

fn engine(root: &Path, phase: &str, safe: bool) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "execution_child", "--ignored", "--nocapture"])
        .env(ROOT, root)
        .env(PHASE, phase)
        .env(RETRY, safe.to_string());
    cmd
}

fn kill_at_four(safe: bool) {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    let journal = root.join("journal");
    let mut first = Process(engine(root, "initial", safe).spawn().unwrap());
    first.wait_until(|| root.join("kill-now").exists());
    first.kill();
    assert_eq!(fs::read(root.join("invocations")).unwrap(), [1, 2, 3, 4]);
    assert_eq!(fs::read(root.join("effect-4")).unwrap(), b"x");

    // Boot the actual daemon binary; a successful STATUS must not precede reconciliation.
    let socket = root.join("s");
    let mut daemon = Process(
        Command::new(env!("CARGO_BIN_EXE_eon-daemon"))
            .args([&journal, &socket])
            .spawn()
            .unwrap(),
    );
    daemon.wait_until(|| socket.exists());
    let status = eon_daemon::request(&socket, "STATUS").unwrap();
    let frames = replay(&journal).unwrap();
    assert!(status.contains(&format!(
        "last durable seq: {}",
        frames.last().unwrap().seq_no
    )));
    let last = eon_kernel::journal_sink::decode(&frames.last().unwrap().payload).unwrap();
    assert!(
        matches!(last, eon_kernel::events::KernelEvent::TaskTransition { task, to, .. }
        if task == id(4) && to == if safe { TaskState::Ready } else { TaskState::UncertainFailure })
    );
    assert!(!journal.join("task-staging").join(id(4).as_str()).exists());
    assert!(
        eon_daemon::boot(&journal).is_err(),
        "daemon must retain exclusive ownership"
    );
    daemon.kill();

    assert!(engine(root, "resume", safe).status().unwrap().success());
    let before = replay(&journal).unwrap();
    assert!(engine(root, "resume", safe).status().unwrap().success());
    assert_eq!(
        replay(&journal).unwrap(),
        before,
        "settled reboot appended more work"
    );
    let boot = eon_daemon::boot(&journal).unwrap();
    for n in 1..=8 {
        let state = match (safe, n) {
            (false, 4) => TaskState::UncertainFailure,
            (false, 8) => TaskState::Pending,
            _ => TaskState::Completed,
        };
        assert_eq!(boot.state().state(&id(n)), Some(state));
        let effect = root.join(format!("effect-{n}"));
        if state == TaskState::Pending {
            assert!(!effect.exists(), "descendant ran past an uncertain outcome");
        } else {
            assert_eq!(
                fs::read(effect).unwrap(),
                b"x",
                "external effect {n} repeated/lost"
            );
        }
    }
    let expected: &[u8] = if safe {
        &[1, 2, 3, 4, 4, 5, 6, 7, 8]
    } else {
        &[1, 2, 3, 4, 5, 6, 7]
    };
    assert_eq!(fs::read(root.join("invocations")).unwrap(), expected);
}

#[test]
fn kill_at_four_resumes_an_idempotent_effect_and_completes_all_eight() {
    kill_at_four(true);
}

#[test]
fn kill_at_four_never_repeats_a_non_idempotent_effect_or_releases_its_descendant() {
    kill_at_four(false);
}
