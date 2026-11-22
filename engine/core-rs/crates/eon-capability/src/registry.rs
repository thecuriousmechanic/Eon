//! Local dispatch for explicitly approved, import-free Wasm tools. Only the
//! trusted embedder supplies the allowlist, labels, task profile and masked runner.

use std::collections::BTreeMap;
use std::sync::Arc;

use eon_foundation::{group_commit::GroupCommitJournal, scrubber::redaction_marker};
use eon_primitives::{EonError, EonResult};
use serde_json::Value;

use crate::ifc::labels::{CognitiveTaint, SecurityLabel};
use crate::ifc::rule_of_two::{Capability, Decision, TaskProfile, decide};
use crate::mcp_pinning::{PinStore, ToolDescriptor};
use crate::melon::{self, MaskedRunner, ToolCall};
use crate::schema;
use crate::wasm_tool::{MAX_INPUT_BYTES, MAX_MODULE_BYTES, WasmWorker, check_size};

mod approval;
pub use approval::DurableApprovals;

/// Provisioned independently of registration (never computed from a listing
/// as a way to approve it). The digest covers the exact binary or WAT bytes.
pub struct AllowedTool {
    pub descriptor: ToolDescriptor,
    pub module_blake3: [u8; 32],
}

impl AllowedTool {
    fn identity(&self) -> [u8; 32] {
        let mut hash = blake3::Hasher::new_derive_key("eon-capability durable tool approval v1");
        hash.update(&crate::mcp_pinning::fingerprint(&self.descriptor));
        hash.update(&self.module_blake3);
        hash.finalize().into()
    }
}

struct ApprovedIdentity {
    module: [u8; 32],
    approval: [u8; 32],
}

pub struct ToolInput<'a> {
    pub json: &'a str,
    pub label: &'a SecurityLabel,
}

#[derive(Debug)]
pub struct ToolOutcome {
    pub output: String,
    pub label: SecurityLabel,
}

impl ToolOutcome {
    pub fn taint(&self) -> CognitiveTaint {
        self.label.integrity.into()
    }
}

struct RegisteredTool {
    descriptor: ToolDescriptor,
    schema: schema::CheckedSchema,
    module: Box<[u8]>,
}

#[derive(Default)]
pub struct CapabilityRegistry {
    allowed: BTreeMap<String, ApprovedIdentity>,
    durable: Option<DurableApprovals>,
    tools: BTreeMap<String, RegisteredTool>,
    pins: PinStore,
    melon: Option<Box<dyn MaskedRunner>>,
    worker: Option<WasmWorker>,
    journal: Option<Arc<GroupCommitJournal>>,
}

impl CapabilityRegistry {
    /// An empty registry allows no tools.
    pub fn new() -> Self {
        Self::default()
    }

    /// Provision a worker with `with_wasm_worker` before registering Wasm.
    pub fn with_allowlist(
        allowed: impl IntoIterator<Item = AllowedTool>,
        durable: DurableApprovals,
    ) -> EonResult<Self> {
        let mut registry = Self {
            durable: Some(durable),
            ..Self::new()
        };
        for entry in allowed {
            let name = &entry.descriptor.name;
            let identity = ApprovedIdentity {
                module: entry.module_blake3,
                approval: entry.identity(),
            };
            if name.is_empty() || registry.allowed.insert(name.clone(), identity).is_some() {
                return Err(EonError::ValidationError(
                    "empty or duplicate allowlist identity".into(),
                ));
            }
            registry.pins.verify(&entry.descriptor)?;
        }
        Ok(registry)
    }

    /// Trusted host configuration, never a descriptor or tool argument.
    pub fn with_wasm_worker(mut self, worker: WasmWorker) -> Self {
        self.worker = Some(worker);
        self
    }

    /// Host-owned durable journal outside the workspace. Both dispatch APIs
    /// require an acknowledged attempt before admission and an acknowledged
    /// outcome before returning. An unmatched attempt is not a success.
    pub fn with_dispatch_journal(mut self, journal: Arc<GroupCommitJournal>) -> Self {
        self.journal = Some(journal);
        self
    }

    fn worker(&self) -> EonResult<&WasmWorker> {
        self.worker.as_ref().ok_or_else(|| {
            EonError::SecurityViolation("registered Wasm requires an isolated worker".into())
        })
    }

    fn durable(&self) -> EonResult<&DurableApprovals> {
        self.durable
            .as_ref()
            .ok_or_else(|| EonError::SecurityViolation("durable approval store required".into()))
    }

    /// Metadata or code drift quarantines even the previously registered version.
    /// Only approved source bytes enter the worker. No compiled code crosses IPC.
    pub fn register(&mut self, descriptor: ToolDescriptor, module: &[u8]) -> EonResult<()> {
        let approved = self
            .allowed
            .get(&descriptor.name)
            .ok_or_else(|| EonError::SecurityViolation("tool is not allowlisted".into()))?;
        if self.pins.is_quarantined(&descriptor.name) {
            return Err(EonError::SecurityViolation("tool is quarantined".into()));
        }
        let record = self
            .durable()?
            .acquire(&descriptor.name, &approved.approval)?;
        record.consume()?;
        self.pins.verify(&descriptor)?;
        if let Err(limit) = check_size("tool_module_bytes", module.len(), MAX_MODULE_BYTES) {
            self.pins.quarantine_module(&descriptor.name);
            return Err(limit);
        }
        if blake3::hash(module).as_bytes() != &approved.module {
            return Err(self.pins.quarantine_module(&descriptor.name));
        }
        let checked = if !self.tools.contains_key(&descriptor.name) {
            let schema = serde_json::from_str(&descriptor.input_schema)
                .map_err(|e| EonError::ValidationError(format!("invalid tool schema: {e}")))?;
            let schema = schema::CheckedSchema::new(schema).map_err(EonError::ValidationError)?;
            self.worker()?.validate(module)?;
            Some(RegisteredTool {
                descriptor,
                schema,
                module: module.into(),
            })
        } else {
            None
        };
        record.restore(&approved.approval)?;
        if let Some(tool) = checked {
            self.tools.insert(tool.descriptor.name.clone(), tool);
        }
        Ok(())
    }

    pub fn set_melon_runner(&mut self, runner: Box<dyn MaskedRunner>) {
        self.melon = Some(runner);
    }

    pub fn dispatch(
        &mut self,
        name: &str,
        profile: &mut TaskProfile,
        input: ToolInput<'_>,
    ) -> EonResult<ToolOutcome> {
        self.dispatch_guarded(name, profile, input, None)
    }

    /// `profile.context` is the workflow ledger: observing external data taints it
    /// even if admission fails. An untrusted proposal requires the actual preceding
    /// context and a successful masked check; plain dispatch is not a bypass.
    pub fn dispatch_guarded(
        &mut self,
        name: &str,
        profile: &mut TaskProfile,
        input: ToolInput<'_>,
        untrusted_context: Option<&str>,
    ) -> EonResult<ToolOutcome> {
        profile.context = profile.context.join(input.label);
        for parameter in &profile.control_parameters {
            profile.context = profile.context.join(&parameter.label);
        }
        if untrusted_context.is_some() {
            profile.context = profile
                .context
                .join(&SecurityLabel::external("masked context"));
        }
        let journal = Arc::clone(self.journal.as_ref().ok_or_else(|| {
            EonError::SecurityViolation(
                "registered dispatch requires a durable audit journal".into(),
            )
        })?);
        // Never journal arbitrary guest text (including names, error messages,
        // provenance and masked context). A short redaction tag is not an
        // approval identity; use the attempt's sequence to pair its outcome.
        let attempt = journal.append_scrubbed(&format!(
            "tool.dispatch attempt name={}",
            redaction_marker(name)
        ))?;
        let result = self.dispatch_checked(name, profile, input, untrusted_context);
        let status = match &result {
            Ok(_) => "ok",
            Err(EonError::CapabilityNotFound { .. }) => "not_registered",
            Err(EonError::SecurityViolation(_)) => "denied",
            Err(EonError::ValidationError(_)) => "invalid",
            Err(EonError::ResourceExhausted { .. }) => "exhausted",
            Err(EonError::ResourceLeaseTimeout { .. }) => "timeout",
            Err(_) => "failed",
        };
        journal.append_scrubbed(&format!("tool.dispatch outcome attempt={attempt} {status}"))?;
        result
    }

    fn dispatch_checked(
        &mut self,
        name: &str,
        profile: &mut TaskProfile,
        input: ToolInput<'_>,
        untrusted_context: Option<&str>,
    ) -> EonResult<ToolOutcome> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| EonError::CapabilityNotFound {
                capability_id: name.into(),
            })?;
        self.pins.verify(&tool.descriptor)?;
        // Serialize with registrations/re-approvals until guest teardown.
        let identity = self
            .allowed
            .get(name)
            .ok_or_else(|| EonError::SecurityViolation("not allowlisted".into()))?;
        let _approval = self.durable()?.acquire(name, &identity.approval)?;
        if let Decision::RequireRawHumanGate(reason) = decide(profile) {
            return Err(EonError::SecurityViolation(format!(
                "dispatch needs human approval: {reason:?}"
            )));
        }
        let tainted =
            CognitiveTaint::from(profile.context.integrity) == CognitiveTaint::UntrustedExternal;
        if tainted
            && profile
                .capabilities
                .iter()
                .any(|cap| !matches!(cap, Capability::ReadWorkspace))
        {
            return Err(EonError::SecurityViolation(
                "tainted workflow cannot request privileged dispatch".into(),
            ));
        }
        check_size("tool_input_bytes", input.json.len(), MAX_INPUT_BYTES)?;
        let arguments: Value = serde_json::from_str(input.json)
            .map_err(|e| EonError::ValidationError(format!("invalid JSON tool input: {e}")))?;
        tool.schema
            .validate(&arguments)
            .map_err(EonError::ValidationError)?;
        // The guest gets exactly the value that passed schema validation and MELON,
        // not an independently interpreted raw encoding (e.g. duplicate object keys).
        let json = schema::canonical_json(&arguments);
        if tainted {
            let context = untrusted_context.filter(|s| !s.is_empty()).ok_or_else(|| {
                EonError::SecurityViolation("MELON requires untrusted context".into())
            })?;
            check_size("masked_context_bytes", context.len(), MAX_INPUT_BYTES)?;
            let runner = self
                .melon
                .as_deref()
                .ok_or_else(|| EonError::SecurityViolation("MELON runner unavailable".into()))?;
            melon::check(
                runner,
                context,
                &ToolCall {
                    name: name.into(),
                    arguments,
                },
            )?;
        }
        profile.context = profile.context.join(&SecurityLabel::external(name));
        let output = self.worker()?.run(&tool.module, &json)?;
        Ok(ToolOutcome {
            output,
            label: profile.context.clone(),
        })
    }
}
