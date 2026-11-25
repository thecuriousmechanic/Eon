//! Process-wide ONNX Runtime environment shared by voice, vision and cognition.

use std::sync::OnceLock;

use ort::environment::Environment;

use crate::{EonError, EonResult};

/// ORT rc.15 permits only one build, even after the last session is dropped.
pub fn environment() -> EonResult<&'static Environment> {
    static ENVIRONMENT: OnceLock<Result<Environment, String>> = OnceLock::new();
    ENVIRONMENT
        .get_or_init(|| {
            ort::init()
                .with_name("eon")
                .with_telemetry(false)
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|reason| EonError::OperationFailed {
            operation: "onnx.environment".into(),
            reason: reason.clone(),
        })
}
