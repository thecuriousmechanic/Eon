//! Linux workstation probes. The self-only sensor never uses the legacy loader.

#[cfg(all(target_os = "linux", feature = "legacy-exec"))]
mod legacy;
#[cfg(all(target_os = "linux", feature = "legacy-exec"))]
pub use legacy::ExecCounter;

#[cfg(all(target_os = "linux", feature = "self-fsync"))]
pub mod self_fsync;
