//! EON Capability Fabric: extensible tool contracts, sandboxing, and providers.
pub mod ifc;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod local_lsp;
pub mod mcp_pinning;
pub mod melon;
pub mod microvm;
#[cfg(any(target_os = "linux", all(target_os = "android", target_arch = "x86_64")))]
pub mod pty;
pub mod quarantine;
pub mod registry;
pub mod research;
pub mod sandbox;
pub mod schema;
pub mod wasm_tool;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod workspace;

pub use eon_primitives::*;
