//! Legacy workstation exec probe.
//!
//! The probe object is built from `engine/bpf/exec_count.bpf.c` by `scripts/build_bpf.sh`. Loading and attaching a
//! tracepoint program needs `CAP_BPF` (on this host, `kernel.unprivileged_bpf_disabled=2`), so the loader runs as root.

use aya::maps::Array;
use aya::programs::TracePoint;
use aya::Ebpf;
use eon_primitives::{EonError, EonResult};

fn bpf_err(operation: &str, reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: format!("ebpf.{operation}"),
        reason: reason.to_string(),
    }
}

/// An attached probe that counts `execve` calls system-wide.
pub struct ExecCounter {
    ebpf: Ebpf,
}

impl ExecCounter {
    /// Loads the compiled probe object, attaches it to `syscalls:sys_enter_execve`, and returns the counter.
    pub fn attach(object: &[u8]) -> EonResult<Self> {
        let mut ebpf = Ebpf::load(object).map_err(|e| bpf_err("load", e))?;
        let program: &mut TracePoint = ebpf
            .program_mut("count_execve")
            .ok_or_else(|| bpf_err("program", "count_execve is missing from the object"))?
            .try_into()
            .map_err(|e| bpf_err("program", e))?;
        program.load().map_err(|e| bpf_err("program.load", e))?;
        program
            .attach("syscalls", "sys_enter_execve")
            .map_err(|e| bpf_err("program.attach", e))?;
        Ok(Self { ebpf })
    }

    /// Number of `execve` calls seen since the probe was attached.
    pub fn count(&self) -> EonResult<u64> {
        let map = self
            .ebpf
            .map("EXEC_COUNT")
            .ok_or_else(|| bpf_err("map", "EXEC_COUNT is missing from the object"))?;
        let counts: Array<_, u64> = Array::try_from(map).map_err(|e| bpf_err("map", e))?;
        counts.get(&0, 0).map_err(|e| bpf_err("map.get", e))
    }
}
