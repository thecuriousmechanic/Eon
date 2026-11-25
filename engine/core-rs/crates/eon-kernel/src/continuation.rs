//! Recorded, local-only expansion of cognitive checkpoints. This is a graph
//! planner, not a capability executor. It never interprets discovery text as
//! child contracts. Both alternative subgraphs come from the trusted host.

use std::collections::BTreeSet;

use eon_primitives::{EonError, EonResult, ExecutionId, TaskId};

use crate::events::{EventSink, KernelEvent, complete_recorded};
use crate::model::{Task, TaskType, Workflow};
use crate::workflow as runtime;

pub const MAX_DISCOVERY_INPUTS: usize = 8;
pub const MAX_READ_BYTES: usize = 1024 * 1024;
pub const MAX_DIAGNOSTICS: usize = 4096;
pub const MAX_CHILDREN: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

pub struct Diagnostic {
    pub severity: Severity,
    /// One-based line number in the local document.
    pub line: std::num::NonZeroU32,
}

/// In-process adapter input; deliberately not deserializable. The caller must
/// attest the result's provenance independently of both its content and the
/// tool name (local files and local tools can still contain untrusted data).
pub enum TrustedLocalOutput<'a> {
    WorkspaceRead { text: &'a str },
    LspDiagnostics { diagnostics: &'a [Diagnostic] },
}

/// All the data retained in working context for one completed discovery.
/// No paths, tool-supplied instructions, source text or diagnostic messages.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DiscoveryDigest {
    Read {
        bytes: usize,
        lines: usize,
        nonblank_lines: usize,
    },
    Diagnostics {
        warnings: usize,
        errors: usize,
        first_error_line: Option<u32>,
    },
}

impl DiscoveryDigest {
    fn has_finding(self) -> bool {
        match self {
            Self::Read { nonblank_lines, .. } => nonblank_lines == 0,
            Self::Diagnostics { errors, .. } => errors != 0,
        }
    }
}

/// Opaque evidence issued only after a trusted completion was recorded in
/// this particular run. The caller can discard it once its consumers finish.
pub struct DiscoveryReceipt {
    run: ExecutionId,
    producer: TaskId,
    digest: DiscoveryDigest,
}

impl DiscoveryReceipt {
    pub fn producer(&self) -> &TaskId {
        &self.producer
    }

    pub fn digest(&self) -> DiscoveryDigest {
        self.digest
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Source {
    Read,
    Diagnostics,
}

fn source(task: &Task) -> Option<Source> {
    match (
        &task.task_type,
        task.capability.as_str(),
        task.operation.as_str(),
    ) {
        (TaskType::AtomicExecution, "workspace.read@v1", "read_range") => Some(Source::Read),
        (TaskType::AtomicExecution, "dev.lsp@v1", "diagnostics") => Some(Source::Diagnostics),
        _ => None,
    }
}

fn compact(task: &Task, output: TrustedLocalOutput<'_>) -> EonResult<DiscoveryDigest> {
    match (source(task), output) {
        (Some(Source::Read), TrustedLocalOutput::WorkspaceRead { text }) => {
            if text.len() > MAX_READ_BYTES {
                return Err(invalid("workspace read exceeds compaction budget"));
            }
            let mut lines = 0;
            let mut nonblank_lines = 0;
            for line in text.lines() {
                lines += 1;
                nonblank_lines += usize::from(!line.trim().is_empty());
            }
            Ok(DiscoveryDigest::Read {
                bytes: text.len(),
                lines,
                nonblank_lines,
            })
        }
        (Some(Source::Diagnostics), TrustedLocalOutput::LspDiagnostics { diagnostics }) => {
            if diagnostics.len() > MAX_DIAGNOSTICS {
                return Err(invalid("LSP diagnostics exceed compaction budget"));
            }
            let mut errors = 0;
            let mut first_error_line = None;
            for entry in diagnostics {
                if entry.severity == Severity::Error {
                    errors += 1;
                    let line = entry.line.get();
                    first_error_line = Some(first_error_line.map_or(line, |n: u32| n.min(line)));
                }
            }
            Ok(DiscoveryDigest::Diagnostics {
                warnings: diagnostics.len() - errors,
                errors,
                first_error_line,
            })
        }
        _ => Err(EonError::SecurityViolation(
            "output does not match an allowed local discovery contract".into(),
        )),
    }
}

/// Pre-authorized local task templates, never populated from discovery
/// payloads. An empty/blank read or an LSP error selects `on_finding`.
#[derive(Clone, Copy)]
pub struct LocalBranches<'a> {
    pub on_finding: &'a [Task],
    pub otherwise: &'a [Task],
}

fn allowed_child(task: &Task) -> bool {
    source(task).is_some()
        || matches!(
            (
                &task.task_type,
                task.capability.as_str(),
                task.operation.as_str(),
            ),
            (TaskType::AtomicExecution, "workspace.patch@v1", "apply")
                | (TaskType::AtomicExecution, "dev.test@v1", "run")
                | (
                    TaskType::CognitiveContinuation,
                    "kernel.continuation@v1",
                    "expand"
                )
        )
}

fn invalid(message: &str) -> EonError {
    EonError::ValidationError(message.into())
}

/// Owns the execution projection so downstream work cannot accidentally be
/// dispatched between the checkpoint completion and its recorded splice.
pub struct ContinuationRun<'sink> {
    id: ExecutionId,
    definition: Workflow,
    graph: runtime::Workflow,
    expanded: BTreeSet<TaskId>,
    sink: &'sink mut dyn EventSink,
    /// A lost acknowledgement does not prove that the record wasn't written.
    stopped: bool,
}

impl<'sink> ContinuationRun<'sink> {
    /// Starts a fresh execution, not a journal recovery operation.
    pub fn new(definition: Workflow, sink: &'sink mut dyn EventSink) -> Self {
        Self {
            id: ExecutionId::new(),
            graph: runtime::Workflow::from(&definition),
            definition,
            expanded: BTreeSet::new(),
            sink,
            stopped: false,
        }
    }

    /// Current validated contracts, including the actual rebound edges.
    pub fn definition(&self) -> &Workflow {
        &self.definition
    }

    fn gate_open(&self, dependency: &TaskId) -> bool {
        self.definition.tasks()[dependency].task_type != TaskType::CognitiveContinuation
            || self.expanded.contains(dependency)
    }

    pub fn ready(&self) -> Vec<TaskId> {
        if self.stopped {
            return Vec::new();
        }
        self.graph
            .ready()
            .into_iter()
            .filter(|id| {
                self.definition.tasks()[id]
                    .dependencies
                    .iter()
                    .all(|dep| self.gate_open(dep))
            })
            .collect()
    }

    /// Recording a normal completion does not attest any data. Completing a
    /// checkpoint alone is also insufficient to release downstream tasks.
    pub fn complete(&mut self, task: &TaskId) -> EonResult<()> {
        if !self.ready().contains(task) {
            return Err(invalid("task is not ready"));
        }
        complete_recorded(&mut self.graph, task, self.sink).inspect_err(|_| self.stopped = true)
    }

    /// Trusted local executor boundary. Compact immediately; a rejected
    /// result or failed completion record issues no receipt.
    pub fn complete_trusted_local(
        &mut self,
        producer: &TaskId,
        output: TrustedLocalOutput<'_>,
    ) -> EonResult<DiscoveryReceipt> {
        let task = self
            .definition
            .tasks()
            .get(producer)
            .ok_or_else(|| invalid("unknown discovery task"))?;
        let digest = compact(task, output)?;
        self.complete(producer)?;
        Ok(DiscoveryReceipt {
            run: self.id.clone(),
            producer: producer.clone(),
            digest,
        })
    }

    /// Each direct dependency naming one of the two allowed discovery
    /// contracts must contribute exactly one receipt. Other dependencies are
    /// ordering constraints only and cannot contribute data.
    fn has_finding(&self, parent: &Task, inputs: &[&DiscoveryReceipt]) -> EonResult<bool> {
        if inputs.is_empty() || inputs.len() > MAX_DISCOVERY_INPUTS {
            return Err(invalid("missing or excessive discovery context"));
        }
        let mut seen = BTreeSet::new();
        for &input in inputs {
            if input.run != self.id
                || !parent.dependencies.contains(&input.producer)
                || !seen.insert(&input.producer)
            {
                return Err(invalid(
                    "foreign, unrelated, or duplicate discovery receipt",
                ));
            }
        }
        if parent
            .dependencies
            .iter()
            .any(|id| source(&self.definition.tasks()[id]).is_some() && !seen.contains(id))
        {
            return Err(invalid("missing named discovery result"));
        }
        Ok(inputs.iter().any(|r| r.digest.has_finding()))
    }

    fn validate_anchors(&self, checkpoint: &Task, children: &[Task]) -> EonResult<()> {
        let mut allowed: BTreeSet<_> = children.iter().map(|t| &t.id).collect();
        let mut ancestors = vec![&checkpoint.id];
        while let Some(id) = ancestors.pop() {
            if allowed.insert(id) {
                ancestors.extend(&self.definition.tasks()[id].dependencies);
            }
        }
        if children
            .iter()
            .flat_map(|t| &t.dependencies)
            .any(|id| !allowed.contains(id))
        {
            return Err(invalid(
                "child dependency is not a sibling or upstream task",
            ));
        }
        Ok(())
    }

    /// Splice a completed checkpoint exactly once in this run. Validate the
    /// full candidate projection before writing the single `SubgraphSpliced`
    /// event; publish neither contracts nor readiness until the sink acks it.
    /// Use `JournalSink` for fsync semantics. The legacy event records IDs,
    /// not executable templates; it does not authorize restart/re-dispatch.
    pub fn expand(
        &mut self,
        parent: &TaskId,
        inputs: &[&DiscoveryReceipt],
        branches: LocalBranches<'_>,
    ) -> EonResult<()> {
        if self.stopped {
            return Err(invalid("execution stopped after sink failure"));
        }
        let checkpoint = self
            .definition
            .tasks()
            .get(parent)
            .ok_or_else(|| invalid("unknown checkpoint"))?;
        if checkpoint.task_type != TaskType::CognitiveContinuation
            || self.graph.get(parent).map(|t| t.state) != Some(runtime::TaskState::Completed)
            || self.expanded.contains(parent)
        {
            return Err(invalid("expected a completed, unexpanded continuation"));
        }
        let children = if self.has_finding(checkpoint, inputs)? {
            branches.on_finding
        } else {
            branches.otherwise
        };
        if children.is_empty() || children.len() > MAX_CHILDREN {
            return Err(invalid("empty or excessive expansion"));
        }
        for child in children {
            if child.workflow_id != *self.definition.id() || !allowed_child(child) {
                return Err(EonError::SecurityViolation(
                    "foreign or disallowed child contract".into(),
                ));
            }
        }
        self.validate_anchors(checkpoint, children)?;
        let mut graph = self.graph.clone();
        graph.splice_subgraph(
            parent,
            children
                .iter()
                .map(|t| runtime::Task::new(t.id.clone(), t.dependencies.iter().cloned()))
                .collect(),
        )?;

        let contracts = self
            .definition
            .tasks()
            .values()
            .chain(children)
            .map(|task| {
                let edges = &graph
                    .get(&task.id)
                    .expect("spliced task exists")
                    .dependencies;
                if *edges == task.dependencies {
                    Ok(task.clone())
                } else {
                    let mut spec = (**task).clone();
                    spec.dependencies.clone_from(edges);
                    Task::try_from(spec)
                }
            })
            .collect::<EonResult<Vec<_>>>()?;
        let definition = Workflow::new(
            self.definition.id().clone(),
            self.definition.objective_id().clone(),
            self.definition.created_at_ns(),
            self.definition.epoch(),
            contracts,
        )?;
        self.sink
            .record(KernelEvent::SubgraphSpliced {
                parent: parent.clone(),
                added: children.iter().map(|t| t.id.clone()).collect(),
            })
            .inspect_err(|_| self.stopped = true)?;
        self.definition = definition;
        self.graph = graph;
        self.expanded.insert(parent.clone());
        Ok(())
    }
}
