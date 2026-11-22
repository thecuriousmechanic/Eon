//! EON Distributed Mesh: spatial election and replicated projection state.
pub mod blob;
pub mod discovery;
pub mod election;
pub mod exchange;
pub mod lan;
pub mod observed;
pub mod paired;
pub mod qos;
pub mod split_inference;
pub mod sync;
pub mod transport;
pub mod wan;

pub use eon_primitives::*;
