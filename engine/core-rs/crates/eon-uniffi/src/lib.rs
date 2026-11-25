//! Permissionless, in-process mobile decision surface. No capture, capability registry or journal handle.

use eon_cognition::profile::ModelProfile;
use eon_primitives::{
    ClaimId, ExecutionId, IdentityId, LeaseId, MacroId, MonotonicInstant, ObjectiveId, QueryId,
    SessionId, TaskId, WorkflowId, id::generate_ulid_string,
};
use eon_voice::endpoint::{Completeness as VoiceCompleteness, silence_threshold_ms};
use eon_voice::{gain::PcmGainController, geometry::VAD_FRAME_SAMPLES};
use std::sync::OnceLock;

#[cfg(target_os = "android")]
mod android_vault;

mod dag;
pub use dag::{DagNode, validate_task_dag};
mod profile_workflow;
pub use profile_workflow::{MobileWorkflowError, MobileWorkflowReceipt, run_profile_workflow};

uniffi::include_scaffolding!("eon_core");

#[derive(Debug)]
pub enum BridgeError {
    InvalidInput { reason: String },
    UnsupportedTask { reason: String },
}

fn invalid(reason: impl Into<String>) -> BridgeError {
    BridgeError::InvalidInput {
        reason: reason.into(),
    }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput { reason } => write!(f, "invalid input: {reason}"),
            Self::UnsupportedTask { reason } => write!(f, "unsupported task: {reason}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    Complete,
    Ambiguous,
    Incomplete,
}

impl From<Completeness> for VoiceCompleteness {
    fn from(value: Completeness) -> Self {
        match value {
            Completeness::Complete => Self::Complete,
            Completeness::Ambiguous => Self::Ambiguous,
            Completeness::Incomplete => Self::Incomplete,
        }
    }
}

/// Returns the `scripts/models.lock.json` profile key for the device memory in GB.
pub fn select_model_profile(memory_gb: f64) -> Result<String, BridgeError> {
    ModelProfile::select_for_memory_gb(memory_gb)
        .map(|profile| profile.lock_key().to_string())
        .map_err(|e| BridgeError::InvalidInput {
            reason: e.to_string(),
        })
}

/// End-of-turn silence threshold in milliseconds for a transcript completeness tier.
pub fn endpoint_silence_ms(completeness: Completeness) -> u32 {
    silence_threshold_ms(completeness.into())
}

/// Same type prefixes and canonical Crockford suffix as the kernel primitives.
pub fn generate_ulid(prefix: String) -> Result<String, BridgeError> {
    if !matches!(
        prefix.as_str(),
        ObjectiveId::PREFIX
            | TaskId::PREFIX
            | WorkflowId::PREFIX
            | LeaseId::PREFIX
            | IdentityId::PREFIX
            | ExecutionId::PREFIX
            | ClaimId::PREFIX
            | SessionId::PREFIX
            | QueryId::PREFIX
            | MacroId::PREFIX
    ) {
        return Err(invalid("unknown kernel ID prefix"));
    }
    Ok(format!("{prefix}_{}", generate_ulid_string()))
}

/// Nanoseconds since the first bridge clock read in this process, not wall-clock or persistent time.
pub fn get_monotonic_time_nanos() -> i64 {
    static ORIGIN: OnceLock<MonotonicInstant> = OnceLock::new();
    ORIGIN
        .get_or_init(MonotonicInstant::now)
        .elapsed()
        .as_nanos()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[derive(Debug)]
pub struct PcmProbe {
    pub sample_count: u32,
    pub applied_gain: f32,
    pub output_peak: f32,
}

pub fn inject_audio_pcm_for_testing(samples: Vec<i16>) -> Result<PcmProbe, BridgeError> {
    if samples.len() != VAD_FRAME_SAMPLES {
        return Err(invalid(format!(
            "expected {VAD_FRAME_SAMPLES} mono i16 samples at 16 kHz"
        )));
    }
    let mut block: Vec<f32> = samples
        .into_iter()
        .map(|x| f32::from(x) / 32768.0)
        .collect();
    let applied_gain = PcmGainController::asr_default().process(&mut block);
    let output_peak = block.iter().map(|x| x.abs()).fold(0.0, f32::max);
    Ok(PcmProbe {
        sample_count: block.len() as u32,
        applied_gain,
        output_peak,
    })
}

#[derive(Debug)]
pub struct TaskResultDto {
    pub task_id: String,
    pub status: String,
    pub outcome_json: String,
    pub error_code: Option<String>,
}

/// Runs a local, read-only diagnostic; never forwards caller-controlled capability names to a runtime.
pub fn execute_task_sync(
    task_id: String,
    capability: String,
    operation: String,
    parameters_json: String,
) -> Result<TaskResultDto, BridgeError> {
    let task_id = TaskId::from_raw(task_id).map_err(|_| invalid("invalid task ID"))?;
    let outcome = match (capability.as_str(), operation.as_str()) {
        ("diagnostics", "select_model_profile") => {
            let memory = diagnostic_parameter(&parameters_json, "memory_gb")?
                .as_f64()
                .ok_or_else(|| invalid("expected numeric memory_gb"))?;
            serde_json::json!({"profile": select_model_profile(memory)?})
        }
        ("diagnostics", "endpoint_silence_ms") => {
            let tier = match diagnostic_parameter(&parameters_json, "completeness")?.as_str() {
                Some("complete") => Completeness::Complete,
                Some("ambiguous") => Completeness::Ambiguous,
                Some("incomplete") => Completeness::Incomplete,
                _ => return Err(invalid("expected a lowercase completeness tier")),
            };
            serde_json::json!({"silence_ms": endpoint_silence_ms(tier)})
        }
        _ => {
            return Err(BridgeError::UnsupportedTask {
                reason: "unsupported read-only diagnostic".into(),
            });
        }
    };
    Ok(TaskResultDto {
        task_id: task_id.to_string(),
        status: "completed".into(),
        outcome_json: outcome.to_string(),
        error_code: None,
    })
}

fn diagnostic_parameter(input: &str, name: &str) -> Result<serde_json::Value, BridgeError> {
    if input.len() > 1024 {
        return Err(invalid("diagnostic input exceeds 1024 bytes"));
    }
    let mut params: serde_json::Value =
        serde_json::from_str(input).map_err(|_| invalid("invalid parameters JSON"))?;
    params
        .as_object_mut()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.remove(name))
        .ok_or_else(|| invalid(format!("expected only {name}")))
}

#[cfg(test)]
mod codegen_tests {
    use super::*;

    #[test]
    fn profile_selection_returns_lock_keys() {
        assert_eq!(select_model_profile(4.0).unwrap(), "p0_mobile_ultralight");
        assert_eq!(select_model_profile(16.0).unwrap(), "p3_workstation_heavy");
    }

    #[test]
    fn invalid_memory_is_a_typed_error_not_a_panic() {
        assert!(matches!(
            select_model_profile(f64::NAN),
            Err(BridgeError::InvalidInput { .. })
        ));
    }

    #[test]
    fn endpoint_thresholds_match_the_voice_crate() {
        assert_eq!(endpoint_silence_ms(Completeness::Complete), 300);
        assert_eq!(endpoint_silence_ms(Completeness::Incomplete), 1150);
    }

    #[test]
    fn canonical_ids_and_clock() {
        let a = generate_ulid("task".into()).unwrap();
        let b = generate_ulid("task".into()).unwrap();
        assert!(a.parse::<TaskId>().is_ok() && a < b);
        assert!(generate_ulid("path/to/file".into()).is_err());
        let first = get_monotonic_time_nanos();
        assert!(get_monotonic_time_nanos() >= first);
    }

    #[test]
    fn test_frame_is_bounded_and_limited() {
        assert!(inject_audio_pcm_for_testing(vec![]).is_err());
        assert!(inject_audio_pcm_for_testing(vec![0; VAD_FRAME_SAMPLES + 1]).is_err());
        let silence = inject_audio_pcm_for_testing(vec![0; VAD_FRAME_SAMPLES]).unwrap();
        assert_eq!(silence.output_peak, 0.0);
        let probe = inject_audio_pcm_for_testing(vec![i16::MIN; VAD_FRAME_SAMPLES]).unwrap();
        assert_eq!(probe.sample_count, 512);
        assert!((probe.output_peak - 0.95).abs() < 1e-6);
        assert_eq!(probe.output_peak, probe.applied_gain);
    }

    #[test]
    fn diagnostic_tasks_fail_closed() {
        let id = TaskId::new().to_string();
        let run = |op: &str, params: &str| {
            execute_task_sync(id.clone(), "diagnostics".into(), op.into(), params.into())
        };
        assert!(matches!(
            run("not_a_diagnostic", "{}"),
            Err(BridgeError::UnsupportedTask { .. })
        ));
        for bad in [
            "not JSON",
            "{}",
            r#"{"memory_gb":4,"command":"extra"}"#,
            r#"{"memory_gb":-1}"#,
        ] {
            assert!(run("select_model_profile", bad).is_err());
        }
        assert!(matches!(
            execute_task_sync(
                WorkflowId::new().to_string(),
                "diagnostics".into(),
                "select_model_profile".into(),
                r#"{"memory_gb":8}"#.into()
            ),
            Err(BridgeError::InvalidInput { .. })
        ));
        let result = run("select_model_profile", r#"{"memory_gb":16}"#).unwrap();
        assert_eq!(result.task_id, id);
        assert_eq!(result.status, "completed");
        assert_eq!(result.outcome_json, r#"{"profile":"p3_workstation_heavy"}"#);
        assert!(result.error_code.is_none());
        assert_eq!(
            run("endpoint_silence_ms", r#"{"completeness":"ambiguous"}"#)
                .unwrap()
                .outcome_json,
            r#"{"silence_ms":800}"#
        );
        for bad in [
            r#"{"completeness":"unknown"}"#,
            r#"{"memory_gb":4}"#,
            r#"{"completeness":"complete","command":"other"}"#,
            &" ".repeat(1025),
        ] {
            assert!(matches!(
                run("endpoint_silence_ms", bad),
                Err(BridgeError::InvalidInput { .. })
            ));
        }
        assert!(matches!(
            execute_task_sync(
                id,
                "workspace.read@v1".into(),
                "read_range".into(),
                "{}".into()
            ),
            Err(BridgeError::UnsupportedTask { .. })
        ));
    }
}
