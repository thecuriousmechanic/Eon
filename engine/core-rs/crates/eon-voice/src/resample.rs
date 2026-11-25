//! Sample-rate conversion for the audio pipeline (docs/implementation/04, resampling stage).
//!
//! Device audio arrives at the hardware rate (often 48 kHz). The VAD and ASR models need 16 kHz. This module converts
//! a mono clip with a windowed-sinc resampler (rubato). The resampler's startup delay is handled by `process_all`, so
//! no leading silence is left in the output.

use eon_primitives::{EonError, EonResult};
use rubato::audioadapter_buffers::direct::{SequentialSlice, SequentialSliceOfVecs};
use rubato::{Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType, WindowFunction};

use crate::geometry::SAMPLE_RATE_HZ;
use crate::ring::PcmConsumer;

const SINC_LEN: usize = 128;
const OVERSAMPLING: usize = 256;
const CUTOFF: f32 = 0.95;
const CHUNK_FRAMES: usize = 1024;
const MAX_RATIO_RELATIVE: f64 = 1.1;

fn invalid(reason: impl Into<String>) -> EonError {
    EonError::ValidationError(reason.into())
}

fn sinc_parameters() -> SincInterpolationParameters {
    SincInterpolationParameters {
        sinc_len: SINC_LEN,
        f_cutoff: Some(CUTOFF),
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: OVERSAMPLING,
        window: WindowFunction::BlackmanHarris2,
    }
}

/// Perception-side reader for one continuous mono device stream. A poll consumes at most
/// one model frame. Fragmented callbacks are left in the ring until a whole frame is ready.
pub struct CaptureFrames<const N: usize> {
    pcm: PcmConsumer,
    conversion: Option<(Async<f32>, Vec<f32>)>,
    startup_drops: u64,
    invalidated: bool,
}

impl<const N: usize> CaptureFrames<N> {
    pub fn new(pcm: PcmConsumer, device_hz: u32) -> EonResult<Self> {
        if N == 0 || !(8_000..=192_000).contains(&device_hz) {
            return Err(invalid("expected a nonempty frame and a device rate in 8..=192 kHz"));
        }
        let conversion = if device_hz == SAMPLE_RATE_HZ {
            None
        } else {
            let sinc = Async::new_sinc(
                f64::from(SAMPLE_RATE_HZ) / f64::from(device_hz),
                1.0,
                &sinc_parameters(),
                N,
                1,
                FixedAsync::Output,
            )
            .map_err(|e| invalid(format!("capture resampler setup: {e}")))?;
            let input = vec![0.0; sinc.input_frames_max()];
            Some((sinc, input))
        };
        let largest_read = conversion.as_ref().map_or(N, |(sinc, _)| sinc.input_frames_max());
        if pcm.capacity() < largest_read {
            return Err(invalid("audio ring too small for one resampler input block"));
        }
        Ok(Self {
            pcm,
            conversion,
            startup_drops: 0,
            invalidated: false,
        })
    }

    /// Before first poll only: both device clocks are now running; no reply has been enqueued.
    #[cfg(feature = "native")]
    pub(crate) fn discard_startup(&mut self) {
        self.startup_drops = self.pcm.dropped();
        self.pcm.flush();
    }

    /// Start at the current producer tail, including when only a partial block is queued.
    /// Unlike startup, a playback boundary cannot excuse a gap or revive an invalid reader.
    #[cfg(feature = "native")]
    pub(crate) fn discard_buffered(&mut self) -> EonResult<()> {
        self.check_continuity()?;
        self.pcm.flush();
        if let Some((sinc, _)) = &mut self.conversion {
            sinc.reset();
        }
        self.check_continuity()
    }

    fn check_continuity(&self) -> EonResult<()> {
        if self.invalidated {
            return Err(invalid("capture frame failed; restart with a fresh stream"));
        }
        if self.pcm.dropped() != self.startup_drops {
            return Err(invalid("audio ring overflow; restart with a fresh stream"));
        }
        Ok(())
    }

    /// Additional source samples required to produce the next complete model frame,
    /// including any lookahead required by the streaming resampler.
    pub fn missing_input_samples(&self) -> EonResult<usize> {
        self.check_continuity()?;
        let needed = self.conversion.as_ref().map_or(N, |(sinc, _)| sinc.input_frames_next());
        Ok(needed.saturating_sub(self.pcm.available()))
    }

    pub fn ready(&self) -> EonResult<bool> {
        Ok(self.missing_input_samples()? == 0)
    }

    pub fn try_read(&mut self) -> EonResult<Option<[f32; N]>> {
        if !self.ready()? {
            return Ok(None);
        }
        let result = self.read_frame();
        self.invalidated = result.is_err();
        result.map(Some)
    }

    fn read_frame(&mut self) -> EonResult<[f32; N]> {
        let mut out = [0.0; N];
        if let Some((sinc, input)) = &mut self.conversion {
            let n = sinc.input_frames_next();
            let input = &mut input[..n];
            self.pcm.pop_slice(input);
            validate_input(input)?;
            let src = SequentialSlice::new(input, 1, n).map_err(|e| invalid(e.to_string()))?;
            let mut dst = SequentialSlice::new_mut(&mut out, 1, N).map_err(|e| invalid(e.to_string()))?;
            sinc.process_into_buffer(&src, &mut dst, None)
                .map_err(|e| invalid(format!("capture resampler: {e}")))?;
            // Bandlimited interpolation can overshoot near full scale.
            out.iter_mut().for_each(|s| *s = s.clamp(-1.0, 1.0));
        } else {
            self.pcm.pop_slice(&mut out);
            validate_input(&out)?;
        }
        self.check_continuity()?;
        Ok(out)
    }
}

fn validate_input(pcm: &[f32]) -> EonResult<()> {
    if pcm.iter().any(|s| !(-1.0..=1.0).contains(s)) {
        return Err(invalid("expected finite normalized device PCM"));
    }
    Ok(())
}

/// Converts mono samples from `from_hz` to `to_hz`. Equal rates return a copy.
pub fn resample_mono(input: &[f32], from_hz: u32, to_hz: u32) -> EonResult<Vec<f32>> {
    if from_hz == 0 || to_hz == 0 {
        return Err(invalid("sample rates must be non-zero"));
    }
    if from_hz == to_hz || input.is_empty() {
        return Ok(input.to_vec());
    }
    let ratio = f64::from(to_hz) / f64::from(from_hz);
    let mut resampler = Async::<f32>::new_sinc(
        ratio,
        MAX_RATIO_RELATIVE,
        &sinc_parameters(),
        CHUNK_FRAMES,
        1,
        FixedAsync::Input,
    )
    .map_err(|e| invalid(format!("resampler setup failed: {e}")))?;

    let channels = vec![input.to_vec()];
    let adapter = SequentialSliceOfVecs::new(&channels, 1, input.len())
        .map_err(|e| invalid(format!("resampler input: {e}")))?;
    let output = resampler
        .process_all(&adapter, input.len(), None)
        .map_err(|e| invalid(format!("resampling failed: {e}")))?;
    Ok(output.take_data())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, rate: u32, seconds: f32) -> Vec<f32> {
        let n = (rate as f32 * seconds) as usize;
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    /// Counts zero crossings in the central part of the signal, which gives the dominant frequency without the edges.
    fn estimated_hz(samples: &[f32], rate: u32) -> f32 {
        let core = &samples[samples.len() / 4..samples.len() * 3 / 4];
        let crossings = core.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        let seconds = core.len() as f32 / rate as f32;
        crossings as f32 / (2.0 * seconds)
    }

    #[test]
    fn a_48k_tone_becomes_a_16k_tone_at_the_same_frequency() {
        let input = tone(440.0, 48_000, 1.0);
        let out = resample_mono(&input, 48_000, 16_000).unwrap();
        let expected_len = 16_000f32;
        assert!((out.len() as f32 - expected_len).abs() < 16.0, "length {}", out.len());
        let hz = estimated_hz(&out, 16_000);
        assert!((hz - 440.0).abs() < 4.0, "estimated {hz} Hz");
    }

    #[test]
    fn equal_rates_are_a_copy_and_bad_rates_are_refused() {
        let input = tone(100.0, 16_000, 0.1);
        assert_eq!(resample_mono(&input, 16_000, 16_000).unwrap(), input);
        assert!(resample_mono(&input, 0, 16_000).is_err());
        assert!(resample_mono(&input, 16_000, 0).is_err());
    }

    #[test]
    fn fragmented_capture_produces_contiguous_fixed_size_model_frames() {
        for rate in [16_000, 44_100, 48_000] {
            let (mut tx, rx) = crate::ring::pcm_ring(1024);
            let mut reader = CaptureFrames::<160>::new(rx, rate).unwrap();
            let source = tone(440.0, rate, 1.0);
            let mut frames = Vec::new();
            for block in source.chunks(137) {
                assert_eq!(tx.push_slice(block), block.len());
                while let Some(frame) = reader.try_read().unwrap() {
                    frames.extend(frame);
                }
            }
            assert!((15_840..=16_000).contains(&frames.len()));
            assert!((estimated_hz(&frames, SAMPLE_RATE_HZ) - 440.0).abs() < 4.0);
            if rate == SAMPLE_RATE_HZ {
                assert_eq!(frames, source);
            }
        }
    }

    #[test]
    fn a_gap_is_an_error_not_an_inference_frame() {
        let (mut tx, rx) = crate::ring::pcm_ring(160);
        let mut reader = CaptureFrames::<160>::new(rx, 16_000).unwrap();
        tx.push_slice(&[0.0; 159]);
        assert!(reader.try_read().unwrap().is_none());
        tx.push_slice(&[0.0; 2]);
        assert!(reader.try_read().is_err());
        assert!(reader.ready().is_err());
    }

    #[cfg(feature = "native")]
    #[test]
    fn boundary_discard_removes_a_fragment_and_sinc_history_but_not_an_overflow() {
        let (mut tx, rx) = crate::ring::pcm_ring(1024);
        let mut reader = CaptureFrames::<160>::new(rx, 48_000).unwrap();
        tx.push_slice(&[0.75; 1024]);
        assert!(reader.try_read().unwrap().unwrap().iter().any(|&s| s != 0.0));
        reader.discard_buffered().unwrap();
        assert!(!reader.ready().unwrap());
        assert_eq!(tx.push_slice(&[0.; 1024]), 1024);
        assert_eq!(reader.try_read().unwrap().unwrap(), [0.; 160]);
        assert!(tx.push_slice(&[0.; 1024]) < 1024);
        assert!(reader.discard_buffered().is_err());
        assert!(reader.ready().is_err());
    }

    #[test]
    fn invalid_device_pcm_cannot_be_spliced_into_the_next_valid_frame() {
        let (mut tx, rx) = crate::ring::pcm_ring(1024);
        let mut reader = CaptureFrames::<160>::new(rx, 48_000).unwrap();
        tx.push_slice(&[f32::NAN; 1024]);
        assert!(reader.try_read().is_err());
        assert!(reader.ready().is_err());
    }
}
