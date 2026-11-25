//! EON Ciel Cognition: model profiles, local inference, hybrid memory ranking, and memory governance.
#[cfg(feature = "llama-backend")]
pub mod backend;
#[cfg(feature = "objective-compiler")]
pub mod compiler;
pub mod grammar;
#[cfg(feature = "local-inference")]
pub mod melon;
pub mod memory;
pub mod persona;
pub mod profile;
pub mod prompts;
pub mod router;

pub use eon_primitives::*;
