//! Compile explicit reads against one caller-authorized workspace. Keywords are
//! ASCII case-insensitive, paths are case-sensitive, and `then` sequences reads:
//!
//! ```text
//! read file README.md then read 80 bytes from src/lib.rs at offset 128
//! ```
//!
//! `read file` requests the first 4096 bytes; `at offset` defaults to zero.
//! The Linux candidate APIs also accept two single reads separated by `or`,
//! including the shorthand `read file A or B`. Neither is selected by default.
//! Unrecognized input is never forwarded to an inference engine, shell or tool.

#[cfg(target_os = "linux")]
mod candidates;
#[cfg(target_os = "linux")]
mod checkout;
#[cfg(all(test, target_os = "linux"))]
#[path = "../tests/compiler_tests/fixture.rs"]
mod fixture;
mod preview;
#[cfg(target_os = "linux")]
mod snapshot;
#[cfg(target_os = "linux")]
mod workspace;
#[cfg(target_os = "linux")]
mod worktree;

use eon_kernel::model::{Objective, Task, TaskSpec, TaskType, Workflow};
use eon_kernel::resource::{CanonicalResourceKey, Demand, Requirement, ResourceRank};
use eon_primitives::{EonError, EonResult, TaskId, WorkflowId};
use serde_json::json;

#[cfg(target_os = "linux")]
pub use candidates::{
    CandidateDecision, CandidatePresentation, CandidatePreview, preview_candidates,
};
#[cfg(target_os = "linux")]
pub use checkout::{
    CheckoutChoice, CheckoutPresentation, PrivateCheckouts, SelectedCheckout, checkout_candidates,
};
pub use preview::{
    Attention, CONFIRMATION_WINDOW, Confirmation, ConfirmedPreview, Decision, ShadowRead,
    StagedPreview,
};
#[cfg(target_os = "linux")]
pub use snapshot::ReadSnapshot;
#[cfg(target_os = "linux")]
pub use workspace::ReadWorkspace;
#[cfg(target_os = "linux")]
pub use worktree::{CheckoutBacking, CheckoutDirectory, WorktreeStore};

pub const MAX_INTENT_BYTES: usize = 2048;
pub const MAX_READS: usize = 8;
pub const MAX_READ_BYTES: usize = 4096;

/// A range is resolved against a workspace root supplied by the host, never by the objective.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadRequest {
    pub task_id: TaskId,
    pub path: String,
    pub offset: u64,
    pub length: usize,
}

/// No deserialization or raw-workflow constructor: kernel DAG validation alone
/// does not authorize an arbitrary capability or its parameters.
pub struct CompiledObjective {
    workflow: Workflow,
    reads: Vec<ReadRequest>,
}

impl CompiledObjective {
    pub fn reads(&self) -> &[ReadRequest] {
        &self.reads
    }
}

/// Compile the whole objective or reject it. No partial plan escapes, even if a
/// later clause is unsupported. Only IDs are freshly allocated on each submission.
pub fn compile(objective: &Objective) -> EonResult<CompiledObjective> {
    compile_words(objective, &intent_words(&objective.description)?)
}

fn intent_words(intent: &str) -> EonResult<Vec<&str>> {
    if intent.len() > MAX_INTENT_BYTES || !intent.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
    {
        return Err(invalid("expected a bounded single-line ASCII intent"));
    }
    Ok(intent.split_ascii_whitespace().collect())
}

fn compile_words(objective: &Objective, words: &[&str]) -> EonResult<CompiledObjective> {
    let mut reads = Vec::new();
    let mut tasks = Vec::new();
    let workflow_id = WorkflowId::new();
    let mut predecessor = None;
    for clause in words.split(|w| w.eq_ignore_ascii_case("then")) {
        if reads.len() >= MAX_READS {
            return Err(invalid("too many read steps"));
        }
        let read = parse_read(clause)?;
        tasks.push(Task::try_from(TaskSpec {
            id: read.task_id.clone(),
            workflow_id: workflow_id.clone(),
            task_type: TaskType::AtomicExecution,
            capability: "workspace.read@v1".into(),
            operation: "read_range".into(),
            parameters: json!({"path": read.path, "offset": read.offset, "length": read.length}),
            dependencies: predecessor.take().into_iter().collect(),
            resource_requirements: vec![Requirement {
                key: CanonicalResourceKey {
                    rank: ResourceRank::FileLock,
                    id: format!("workspace:///{}", read.path),
                },
                demand: Demand::SharedRead,
            }],
            timeout_ms: 1000,
            is_idempotent: true,
        })?);
        predecessor = Some(read.task_id.clone());
        reads.push(read);
    }
    let workflow = Workflow::new(
        workflow_id,
        objective.id.clone(),
        objective.created_at_ns,
        objective.epoch,
        tasks,
    )?;
    Ok(CompiledObjective { workflow, reads })
}

fn invalid(message: &'static str) -> EonError {
    EonError::ValidationError(message.into())
}

fn parse_read(words: &[&str]) -> EonResult<ReadRequest> {
    let [verb, rest @ ..] = words else {
        return Err(invalid("missing read step"));
    };
    if !verb.eq_ignore_ascii_case("read") {
        return Err(invalid("only explicit file reads are supported"));
    }
    let (path, offset, length) = match rest {
        [file, path] if file.eq_ignore_ascii_case("file") => (*path, 0, MAX_READ_BYTES),
        [count, bytes, from, path, tail @ ..]
            if bytes.eq_ignore_ascii_case("bytes") && from.eq_ignore_ascii_case("from") =>
        {
            let length = number(count)?;
            if !(1..=MAX_READ_BYTES as u64).contains(&length) {
                return Err(invalid("byte count exceeds the permitted range"));
            }
            let offset = match tail {
                [] => 0,
                [at, offset, n]
                    if at.eq_ignore_ascii_case("at") && offset.eq_ignore_ascii_case("offset") =>
                {
                    number(n)?
                }
                _ => return Err(invalid("expected an optional byte offset")),
            };
            (*path, offset, length as usize)
        }
        _ => return Err(invalid("unsupported or ambiguous read intent")),
    };
    // Keep the emitted relative path unambiguous on Unix and Windows. No dot
    // components, hidden files, device names, escape sequences or URI schemes.
    if !public_path(path) {
        return Err(invalid(
            "expected a non-protected relative workspace file path",
        ));
    }
    Ok(ReadRequest {
        task_id: TaskId::new(),
        path: path.into(),
        offset,
        length,
    })
}

fn public_path(path: &str) -> bool {
    path.len() <= 240
        && path
            .split('/')
            .all(|c| valid_component(c) && !protected_name(c))
}

// Defense in depth, not a way to classify arbitrary contents as non-secret.
// The filesystem entry point also requires independent, exact host approval.
fn protected_name(component: &str) -> bool {
    let name = component.to_ascii_lowercase();
    name.starts_with('.')
        || name.ends_with(".git")
        || name.starts_with("id_rsa")
        || name.starts_with("id_dsa")
        || name.starts_with("id_ecdsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("ssh_host_")
        || name.split(['.', '-', '_']).any(|part| {
            matches!(
                part,
                "env"
                    | "credential"
                    | "credentials"
                    | "secret"
                    | "secrets"
                    | "password"
                    | "passwd"
                    | "token"
                    | "tokens"
                    | "key"
                    | "pem"
                    | "p12"
                    | "pfx"
                    | "keystore"
                    | "jks"
                    | "netrc"
            )
        })
}

fn valid_component(s: &str) -> bool {
    if !s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        || s.ends_with('.')
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return false;
    }
    let stem = s.split('.').next().unwrap_or_default().to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "NUL" | "AUX" | "PRN")
        && !matches!(
            stem.as_bytes(),
            [b'C', b'O', b'M', b'0'..=b'9'] | [b'L', b'P', b'T', b'0'..=b'9']
        )
}

fn number(s: &str) -> EonResult<u64> {
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("expected an unsigned decimal number"));
    }
    s.parse()
        .map_err(|_| invalid("invalid byte count or offset"))
}
