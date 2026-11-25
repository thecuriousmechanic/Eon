//! Silero VAD v5 inference over ONNX Runtime (docs/implementation/04, sections 2 and 3).
//!
//! The model takes 512 samples (32 ms at 16 kHz) plus a recurrent state, and returns a speech probability and the
//! next state. The state must be carried between frames of one stream, so each stream owns its own `SileroVad`.

use std::path::Path;
use std::time::{Duration, Instant};

use eon_primitives::{EonError, EonResult};
use ort::session::Session;
use ort::value::Tensor;

use crate::endpoint::{Completeness, Endpoint, Endpointer, RESUME_HOLDOFF_MS};
use crate::geometry::{SAMPLE_RATE_HZ, VAD_FRAME_SAMPLES};
use crate::resample::CaptureFrames;

const STATE_LEN: usize = 2 * 128;
const CONTEXT: usize = 64;
const FRAME_MS: u32 = crate::geometry::ms_for_samples(VAD_FRAME_SAMPLES, SAMPLE_RATE_HZ);
const ACOUSTIC_PAUSE_MS: u32 = 250;
const MAX_PARTIAL_BYTES: usize = 4096;

/// A short acoustic pause can be used to prepare a response. Only the longer
/// semantic `turn_end` closes a listening turn; neither event authorizes an action.
#[derive(Debug, Clone, Copy)]
pub struct ScoredVadFrame {
    pub probability: f32,
    /// Energy of the same captured frame, independent of the model's classification.
    pub rms: f32,
    /// Includes input validation, ONNX evaluation and recurrent state update. Excludes waiting for audio.
    pub inference_time: Duration,
    pub acoustic_pause: Option<Endpoint>,
    pub turn_end: Option<Endpoint>,
}

#[derive(Default)]
struct Activity {
    onset_frames: u8,
    active: bool,
    quiet_ms: u32,
}

impl Activity {
    fn observe(&mut self, p: f32) -> (bool, Option<Endpoint>) {
        if !self.active {
            self.onset_frames = if p >= 0.60 { self.onset_frames + 1 } else { 0 };
            if self.onset_frames < 2 {
                return (false, None);
            }
            self.active = true;
        }
        if p >= 0.35 {
            self.quiet_ms = 0;
            return (true, None);
        }
        if self.quiet_ms >= ACOUSTIC_PAUSE_MS {
            return (false, None);
        }
        self.quiet_ms += FRAME_MS;
        if self.quiet_ms >= ACOUSTIC_PAUSE_MS {
            return (
                false,
                Some(Endpoint {
                    silence_ms: self.quiet_ms,
                }),
            );
        }
        (false, None)
    }
}

/// Endpoint history for one listening turn, independent of the Silero inference session.
#[derive(Default)]
pub struct VadTurnState {
    activity: Activity,
    endpoint: Endpointer,
}

impl VadTurnState {
    /// Feed consecutive Silero probabilities, each with the latest ASR partial *including* that frame.
    pub fn push(&mut self, probability: f32, partial: &str) -> Option<Endpoint> {
        self.observe(probability, partial).1
    }

    fn observe(&mut self, probability: f32, partial: &str) -> (Option<Endpoint>, Option<Endpoint>) {
        let (speech, acoustic_pause) = self.activity.observe(probability);
        // A long partial must not turn a single DSP worker tick into an
        // unbounded text scan. Fall back to the conservative continuation tier.
        let tier = if partial.len() <= MAX_PARTIAL_BYTES {
            Completeness::from_partial(partial)
        } else {
            Completeness::Incomplete
        };
        let turn_end = self.endpoint.push_frame(FRAME_MS, speech, tier);
        if turn_end.is_some() {
            self.activity = Activity::default();
        }
        (acoustic_pause, turn_end)
    }
}

/// Native VAD/endpointer for listening turns. Barge-in detection during active
/// playback runs separately; this holdoff starts only *after* a reply finishes.
pub struct VadEndpoint {
    vad: SileroVad,
    turn: VadTurnState,
    holdoff_samples: usize,
    session: eon_primitives::SessionId,
}

/// Distinguishes an incomplete device frame from 32 ms of processed audio.
#[derive(Debug, PartialEq, Eq)]
pub enum VadStep {
    Waiting,
    Processed(Option<Endpoint>),
}

/// Bind the model to one *already explicitly opened* CPAL input. This worker
/// never opens a device. Each poll runs at most one inference; a discontinuity
/// invalidates this stream instead of stitching unrelated audio into its state.
#[cfg(feature = "native")]
pub struct CaptureVad {
    capture: CaptureFrames<VAD_FRAME_SAMPLES>,
    device: crate::device::CaptureHandle,
    endpoint: VadEndpoint,
    failed: bool,
}

#[cfg(feature = "native")]
impl CaptureVad {
    pub fn new(vad: SileroVad, input: (CaptureFrames<VAD_FRAME_SAMPLES>, crate::device::CaptureHandle)) -> Self {
        Self {
            capture: input.0,
            device: input.1,
            endpoint: VadEndpoint::new(vad),
            failed: false,
        }
    }

    /// Returns `None` when a complete continuous frame is not yet available.
    pub fn poll(&mut self, partial: &str) -> EonResult<Option<ScoredVadFrame>> {
        let discontinuity = || {
            use std::sync::atomic::Ordering::Acquire;
            self.device.counters.driver_errors.load(Acquire) != 0
                || self.device.counters.capture_drops.load(Acquire) != 0
        };
        if self.failed || discontinuity() {
            self.failed = true;
            return Err(onnx_err(
                "capture",
                "capture failed; reopen the input and restart the VAD stream",
            ));
        }
        let result = self.capture.try_read().and_then(|frame| match frame {
            None => Ok(None),
            Some(frame) => self.endpoint.observe(&frame, partial),
        });
        self.failed = result.is_err() || discontinuity();
        if self.failed && result.is_ok() {
            return Err(onnx_err(
                "capture",
                "driver error during inference; restart the VAD stream",
            ));
        }
        result
    }
}

impl VadEndpoint {
    pub fn new(mut vad: SileroVad) -> Self {
        vad.reset();
        Self {
            vad,
            turn: VadTurnState::default(),
            holdoff_samples: 0,
            session: eon_primitives::SessionId::new(),
        }
    }

    pub fn session(&self) -> &eon_primitives::SessionId {
        &self.session
    }

    pub fn reply_finished(&mut self) {
        self.vad.reset();
        self.turn = VadTurnState::default();
        self.holdoff_samples = crate::geometry::samples_for_ms(RESUME_HOLDOFF_MS, SAMPLE_RATE_HZ);
        self.session = eon_primitives::SessionId::new();
    }

    /// Run on the perception worker. Never pads a short callback with fabricated silence.
    pub fn poll_capture(
        &mut self,
        capture: &mut CaptureFrames<VAD_FRAME_SAMPLES>,
        partial: &str,
    ) -> EonResult<VadStep> {
        match capture.try_read()? {
            None => Ok(VadStep::Waiting),
            Some(frame) => self.push(&frame, partial).map(VadStep::Processed),
        }
    }

    pub fn push(&mut self, frame: &[f32; VAD_FRAME_SAMPLES], partial: &str) -> EonResult<Option<Endpoint>> {
        Ok(self.observe(frame, partial)?.and_then(|frame| frame.turn_end))
    }

    /// Score exactly one contiguous frame, or skip it during post-reply holdoff.
    /// All durations use the sample clock, not the worker's polling frequency.
    pub fn observe(&mut self, frame: &[f32; VAD_FRAME_SAMPLES], partial: &str) -> EonResult<Option<ScoredVadFrame>> {
        if self.holdoff_samples != 0 {
            validate_pcm(frame)?;
            self.holdoff_samples = self.holdoff_samples.saturating_sub(frame.len());
            return Ok(None);
        }
        let started = Instant::now();
        let probability = self.vad.speech_probability(frame)?;
        let inference_time = started.elapsed();
        let rms = (frame.iter().map(|x| x * x).sum::<f32>() / VAD_FRAME_SAMPLES as f32).sqrt();
        let (acoustic_pause, turn_end) = self.turn.observe(probability, partial);
        Ok(Some(ScoredVadFrame {
            probability,
            rms,
            inference_time,
            acoustic_pause,
            turn_end,
        }))
    }
}

pub struct SileroVad {
    session: Session,
    input: Tensor<f32>,
    state: Tensor<f32>,
    sr: Tensor<i64>,
}

fn onnx_err(context: &str, e: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: format!("silero.{context}"),
        reason: e.to_string(),
    }
}

fn validate_pcm(frame: &[f32]) -> EonResult<()> {
    if !frame.iter().all(|sample| (-1.0..=1.0).contains(sample)) {
        return Err(EonError::ValidationError("expected finite, normalized mono PCM".into()));
    }
    Ok(())
}

impl SileroVad {
    /// Loads the model from `path`. The file is expected to be the digest-verified artifact from `models.lock.json`.
    pub fn load(path: impl AsRef<Path>) -> EonResult<Self> {
        let session = Session::builder(crate::runtime::environment()?)
            .map_err(|e| onnx_err("builder", e))?
            .with_intra_threads(1)
            .map_err(|e| onnx_err("threads", e))?
            .commit_from_file(path)
            .map_err(|e| onnx_err("load", e))?;
        let mut vad = Self {
            session,
            input: Tensor::from_array((
                [1usize, CONTEXT + VAD_FRAME_SAMPLES],
                vec![0.0; CONTEXT + VAD_FRAME_SAMPLES],
            ))
            .map_err(|e| onnx_err("input", e))?,
            state: Tensor::from_array(([2usize, 1, 128], vec![0.0; STATE_LEN])).map_err(|e| onnx_err("state", e))?,
            sr: Tensor::from_array(((), vec![i64::from(SAMPLE_RATE_HZ)])).map_err(|e| onnx_err("sr", e))?,
        };
        // Pay ORT's first-run setup before opening a stream. Do not retain warmup history.
        vad.speech_probability(&[0.0; VAD_FRAME_SAMPLES])?;
        vad.reset();
        Ok(vad)
    }

    /// Resets the recurrent state, for example at the start of a new utterance stream.
    pub fn reset(&mut self) {
        self.state.extract_tensor_mut().1.fill(0.0);
        self.input.extract_tensor_mut().1.fill(0.0);
    }

    /// Returns the speech probability in [0, 1] for one 32 ms frame of 16 kHz mono samples.
    pub fn speech_probability(&mut self, frame: &[f32]) -> EonResult<f32> {
        if frame.len() != VAD_FRAME_SAMPLES {
            return Err(EonError::ValidationError(format!(
                "Silero v5 needs {VAD_FRAME_SAMPLES} samples per frame, got {}",
                frame.len()
            )));
        }
        validate_pcm(frame)?;
        // The v5 wrapper prepends the previous frame's 64-sample STFT context at 16 kHz.
        self.input.extract_tensor_mut().1[CONTEXT..].copy_from_slice(frame);
        let outputs = self
            .session
            .run(ort::inputs!["input" => &self.input, "state" => &self.state, "sr" => &self.sr])
            .map_err(|e| onnx_err("run", e))?;
        let (_, probability) = outputs
            .get("output")
            .ok_or_else(|| onnx_err("output", "missing probability"))?
            .try_extract_tensor::<f32>()
            .map_err(|e| onnx_err("output", e))?;
        let [p] = probability else {
            return Err(onnx_err("output", "expected one speech probability"));
        };
        let (_, next_state) = outputs
            .get("stateN")
            .ok_or_else(|| onnx_err("stateN", "missing recurrent state"))?
            .try_extract_tensor::<f32>()
            .map_err(|e| onnx_err("stateN", e))?;
        if !(0.0..=1.0).contains(p) || next_state.len() != STATE_LEN || !next_state.iter().all(|v| v.is_finite()) {
            return Err(onnx_err("output", "invalid probability or recurrent state"));
        }
        self.state.extract_tensor_mut().1.copy_from_slice(next_state);
        self.input.extract_tensor_mut().1[..CONTEXT].copy_from_slice(&frame[VAD_FRAME_SAMPLES - CONTEXT..]);
        Ok(*p)
    }
}

#[cfg(test)]
mod vad_tests {
    use super::*;

    #[test]
    fn onset_requires_two_consecutive_scores_and_pause_requires_eight_unbroken_quiet_frames() {
        let mut activity = Activity::default();
        for score in [0.59, 0.60, 0.35, 0.61] {
            assert_eq!(activity.observe(score), (false, None));
        }
        assert_eq!(activity.observe(0.60), (true, None));
        for _ in 0..7 {
            assert_eq!(activity.observe(0.34), (false, None));
        }
        assert_eq!(activity.observe(0.35), (true, None));
        for _ in 0..7 {
            assert_eq!(activity.observe(0.34), (false, None));
        }
        assert_eq!(activity.observe(0.34), (false, Some(Endpoint { silence_ms: 256 })));
        assert_eq!(activity.observe(0.0), (false, None));
        // Resumed speech cancels the longer pending turn timer immediately.
        assert_eq!(activity.observe(0.50), (true, None));
    }
}
