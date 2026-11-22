//! EON Voice Pipeline: frame geometry, endpointing, VAD and ASR inference, and ASR gain control.
#[cfg(feature = "native")]
pub mod aec;
#[cfg(any(feature = "onnx-speech", feature = "sherpa-onnx"))]
pub mod asr;
pub mod barge_in;
#[cfg(feature = "native")]
pub mod commitment;
#[cfg(feature = "native")]
pub mod coordinator;
pub mod delay;
#[cfg(feature = "native")]
pub mod device;
pub mod endpoint;
#[cfg(feature = "onnx-speech")]
pub mod features;
pub mod gain;
pub mod geometry;
pub mod linear_echo;
pub mod pre_cognition;
pub mod resample;
pub mod ring;
#[cfg(feature = "onnx-speech")]
pub use eon_primitives::onnx as runtime;
#[cfg(feature = "native")]
pub mod speaker;
#[cfg(feature = "native")]
pub mod tts;
#[cfg(feature = "onnx-speech")]
pub mod vad;

pub use eon_primitives::*;
