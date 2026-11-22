//! EON Cyber Engine: program analysis, taint slicing, and armor checks.
pub mod ast;
#[cfg(feature = "solver")]
pub mod branches;
pub mod benign_repair;
pub mod cpg;
pub mod defense;
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_pointer_width = "64"
))]
pub mod dev_dap;
pub mod function_review;
pub mod interproc;
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_pointer_width = "64"
))]
pub mod owned_dap;
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_pointer_width = "64"
))]
pub mod owned_debugpy;
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_pointer_width = "64"
))]
pub mod owned_jvm;
pub mod remediation;
pub mod sbom;
#[cfg(feature = "solver")]
pub mod solver;
pub mod taint;

pub use eon_primitives::*;
