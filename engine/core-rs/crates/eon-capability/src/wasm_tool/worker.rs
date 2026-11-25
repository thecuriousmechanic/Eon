use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

#[cfg(target_os = "linux")]
use super::{DISPATCH_FUEL, MAX_OUTPUT_BYTES};
use super::{MAX_INPUT_BYTES, MAX_MODULE_BYTES, PureModule, check_size, operation_failed};
use eon_primitives::{EonError, EonResult};

#[cfg(target_os = "linux")]
mod linux;

pub const MAX_WORKER_WALL_TIME: Duration = Duration::from_secs(5);

const VALIDATE: &str = "--eon-validate-v1";
const RUN: &str = "--eon-run-v1";
const VALID: &[u8] = b"valid\n";
const OUTPUT: &[u8] = b"output\n";
const FAILURE: &[u8] = b"failed\n";
const SIZE: &[u8] = b"size\n";

/// One fresh process per validation or invocation. The host must provision the
/// matching `eon-wasm-worker` as a read-only native executable (e.g. 0555).
/// Its inode and every directory component must be root- or effective-UID-owned;
/// no symlinks, writable executable, or group/other-writable directories.
/// The installation and its runtime libraries must remain trusted.
/// Never derive its path or budget from the workspace, environment or listing.
/// The embedder must not enable auto-reaping or independently reap the
/// supervisor's children during an invocation.
pub struct WasmWorker {
    #[cfg(target_os = "linux")]
    executable: std::fs::File,
    #[cfg(target_os = "linux")]
    wall_time: Duration,
    #[cfg(not(target_os = "linux"))]
    _unsupported: std::convert::Infallible,
}

impl WasmWorker {
    /// A positive wall budget (at most five seconds) for the *whole* worker,
    /// including recompilation on dispatch. Linux also caps CPU and address space.
    pub fn new(executable: &Path, wall_time: Duration) -> EonResult<Self> {
        if !executable.is_absolute() || wall_time.is_zero() || wall_time > MAX_WORKER_WALL_TIME {
            return Err(EonError::ValidationError(
                "expected an absolute worker path and bounded positive deadline".into(),
            ));
        }
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                executable: linux::open_executable(executable).map_err(worker_error)?,
                wall_time,
            })
        }
        #[cfg(not(target_os = "linux"))]
        Err(worker_error("isolated Wasm worker unsupported"))
    }

    pub(crate) fn validate(&self, module: &[u8]) -> EonResult<()> {
        self.invoke(module, None).map(drop)
    }

    pub(crate) fn run(&self, module: &[u8], input: &str) -> EonResult<String> {
        self.invoke(module, Some(input))
    }

    fn invoke(&self, module: &[u8], input: Option<&str>) -> EonResult<String> {
        check_size("tool_module_bytes", module.len(), MAX_MODULE_BYTES)?;
        check_size(
            "tool_input_bytes",
            input.unwrap_or("").len(),
            MAX_INPUT_BYTES,
        )?;
        #[cfg(target_os = "linux")]
        {
            let reply = linux::exchange(self, module, input)?;
            decode(&reply, input.is_some())
        }
        #[cfg(not(target_os = "linux"))]
        Err(worker_error("isolated Wasm worker unsupported"))
    }
}

fn worker_error(reason: impl std::fmt::Display) -> EonError {
    operation_failed("wasm.worker", reason)
}

// Only bounded guest output or a fixed failure classification crosses back.
// In particular, do not propagate compiler diagnostics containing source text.
#[cfg(target_os = "linux")]
fn decode(reply: &[u8], run: bool) -> EonResult<String> {
    if !run && reply == VALID {
        return Ok(String::new());
    }
    if run && let Some(output) = reply.strip_prefix(OUTPUT) {
        check_size("tool_output_bytes", output.len(), MAX_OUTPUT_BYTES)?;
        return std::str::from_utf8(output)
            .map(str::to_owned)
            .map_err(|_| worker_error("invalid worker output"));
    }
    if reply == b"imports" {
        return Err(EonError::SecurityViolation(
            "pure tools cannot import host capabilities".into(),
        ));
    }
    if reply == b"abi" {
        return Err(EonError::ValidationError(
            "expected exported memory32 and run(i32) -> i32".into(),
        ));
    }
    if run && reply == b"fuel" {
        return Err(EonError::ResourceExhausted {
            resource: "wasm_fuel".into(),
            requested: DISPATCH_FUEL,
            available: 0,
        });
    }
    if run && let Some(size) = reply.strip_prefix(SIZE).and_then(|b| b.try_into().ok()) {
        check_size(
            "tool_output_bytes",
            u32::from_le_bytes(size) as usize,
            MAX_OUTPUT_BYTES,
        )?;
    }
    if let Some(operation) = reply.strip_prefix(FAILURE)
        && let Ok(
            op @ ("wasm.compile" | "wasm.engine" | "wasm.fuel" | "wasm.instantiate" | "wasm.export"
            | "wasm.input" | "wasm.call" | "wasm.output"),
        ) = std::str::from_utf8(operation)
    {
        return Err(operation_failed(op, "isolated Wasm operation failed"));
    }
    Err(worker_error("invalid or mismatched worker response"))
}

fn serve() -> io::Result<()> {
    let run = match std::env::args_os()
        .nth(1)
        .as_deref()
        .and_then(|s| s.to_str())
    {
        Some(VALIDATE) => false,
        Some(RUN) => true,
        _ => return Err(io::Error::other("unknown worker protocol")),
    };
    let mut stdin = io::stdin().lock();
    let mut length = [0; 4];
    stdin.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_MODULE_BYTES {
        return Err(io::Error::other("module too large"));
    }
    let mut source = vec![0; length];
    stdin.read_exact(&mut source)?;
    let mut input = String::new();
    stdin
        .take(MAX_INPUT_BYTES as u64 + 1)
        .read_to_string(&mut input)?;
    if input.len() > MAX_INPUT_BYTES || (!run && !input.is_empty()) {
        return Err(io::Error::other("unexpected worker input"));
    }
    let result = PureModule::compile(&source).and_then(|module| {
        if run {
            module.run(&input)
        } else {
            Ok(String::new())
        }
    });
    let mut stdout = io::stdout().lock();
    match result {
        Ok(output) if run => {
            stdout.write_all(OUTPUT)?;
            stdout.write_all(output.as_bytes())?;
        }
        Ok(_) => stdout.write_all(VALID)?,
        Err(EonError::SecurityViolation(_)) => stdout.write_all(b"imports")?,
        Err(EonError::ValidationError(_)) => stdout.write_all(b"abi")?,
        Err(EonError::ResourceExhausted {
            resource,
            requested,
            ..
        }) => match resource.as_str() {
            "wasm_fuel" => stdout.write_all(b"fuel")?,
            "tool_output_bytes" => {
                stdout.write_all(SIZE)?;
                stdout.write_all(&(requested as u32).to_le_bytes())?;
            }
            _ => return Err(io::Error::other("unexpected limit")),
        },
        Err(EonError::OperationFailed { operation, .. }) => {
            stdout.write_all(FAILURE)?;
            stdout.write_all(operation.as_bytes())?;
        }
        Err(_) => return Err(io::Error::other("unexpected failure")),
    }
    stdout.flush()
}

/// Stdio entry point for the host-installed worker executable, not a registry API.
#[doc(hidden)]
pub fn worker_main() -> ExitCode {
    if cfg!(target_os = "linux") && serve().is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
