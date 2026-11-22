//! Model-specific Kaldi-compatible 80-bin log-mel front ends.

use eon_primitives::{EonError, EonResult};
use kaldi_native_fbank::online::FeatureComputer;
use kaldi_native_fbank::{FbankComputer, FbankOptions, OnlineFeature};

pub const SAMPLE_RATE_HZ: f32 = 16_000.0;
pub const NUM_MEL_BINS: usize = 80;

/// Computes one 80-dimensional log-mel vector per 10 ms frame of 16 kHz mono audio. Dither is off and edges are not
/// snipped, so the output is deterministic and matches the reference front end.
pub fn kaldi_fbank(samples: &[f32]) -> EonResult<Vec<Vec<f32>>> {
    let mut features = online_fbank()?;
    features.accept_waveform(SAMPLE_RATE_HZ, samples);
    features.input_finished();
    Ok(features.features)
}

pub(crate) fn online_fbank() -> EonResult<OnlineFeature> {
    let mut opts = base_options();
    opts.frame_opts.snip_edges = false;
    extractor(opts)
}

fn base_options() -> FbankOptions {
    let mut opts = FbankOptions::default();
    opts.frame_opts.dither = 0.0;
    opts.mel_opts.num_bins = NUM_MEL_BINS;
    opts.use_energy = false;
    opts
}

fn extractor(opts: FbankOptions) -> EonResult<OnlineFeature> {
    let computer = FbankComputer::new(opts).map_err(|e| EonError::OperationFailed {
        operation: "features.fbank".into(),
        reason: e,
    })?;
    Ok(OnlineFeature::new(FeatureComputer::Fbank(computer)))
}

/// WeSpeaker ResNet34-LM reference (`wespeaker/runtime/core/frontend/{fbank.h,feature_pipeline.cc}`):
/// PCM16 units, complete 25 ms Hamming windows every 10 ms. The signed PCM scale matters
/// *before* the log-energy floor; utterance CMN cannot restore floored bands afterwards.
pub fn wespeaker_fbank(normalized: &[f32]) -> EonResult<Vec<Vec<f32>>> {
    let pcm: Vec<_> = normalized.iter().map(|s| s * 32768.0).collect();
    speaker_fbank(&pcm, "hamming")
}

/// 3D-Speaker's `speakerlab/process/processor.py::FBank` calls torchaudio's
/// Kaldi fbank on normalized waveform samples: Povey windows and snipped edges.
#[cfg(feature = "native")]
pub(crate) fn camplusplus_fbank(normalized: &[f32]) -> EonResult<Vec<Vec<f32>>> {
    speaker_fbank(normalized, "povey")
}

fn speaker_fbank(samples: &[f32], window: &str) -> EonResult<Vec<Vec<f32>>> {
    let mut opts = base_options();
    opts.frame_opts.snip_edges = true;
    opts.frame_opts.window_type = window.into();
    // The pinned extractor's built-in log floors power at 1e-20, not the
    // float epsilon used by both reference front ends on each mel band.
    opts.use_log_fbank = false;
    let mut features = extractor(opts)?;
    features.accept_waveform(SAMPLE_RATE_HZ, samples);
    features.input_finished();
    for power in features.features.iter_mut().flatten() {
        *power = power.clamp(f32::EPSILON, f32::INFINITY).ln();
    }
    Ok(features.features)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wespeaker_silence_and_subfloor_energy_match_the_reference_log_floor() {
        let mut pcm = [0.0; 400];
        for impulse in [0.0, 1e-10] {
            pcm[17] = impulse;
            let frames = wespeaker_fbank(&pcm).unwrap();
            assert_eq!(frames.len(), 1);
            assert!(frames[0].iter().all(|&bin| bin == f32::EPSILON.ln()));
        }
    }

    #[test]
    fn wespeaker_matches_pcm16_hamming_reference_without_changing_asr_edges() {
        let mut pcm = [0.0; 559]; // One complete window; one sample short of a second.
        pcm[17] = 1.0 / 32768.0;
        let frames = wespeaker_fbank(&pcm).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 80);
        // Independent 512-point DFT of a one-count impulse after DC removal,
        // 0.97 pre-emphasis and symmetric Hamming, then triangular Kaldi mel sums.
        for (bin, expected) in [(0, -9.645185), (25, -6.267827), (79, -1.1906404)] {
            assert!((frames[0][bin] - expected).abs() < 1e-4, "{bin}: {}", frames[0][bin]);
        }
        assert_eq!(kaldi_fbank(&pcm).unwrap().len(), 3);
    }

    #[cfg(feature = "native")]
    #[test]
    fn camplusplus_uses_normalized_pcm_povey_and_snipped_edges() {
        let mut pcm = [0.0; 559];
        pcm[17] = 1.0;
        let frames = camplusplus_fbank(&pcm).unwrap();
        assert_eq!(frames.len(), 1);
        // Independent 512-point DFT of a unit impulse after frame DC removal,
        // 0.97 pre-emphasis and a symmetric Hann window raised to 0.85.
        for (bin, expected) in [(0, -10.034028), (25, -8.328039), (79, -3.282064)] {
            assert!((frames[0][bin] - expected).abs() < 1e-4, "{bin}: {}", frames[0][bin]);
        }
        pcm[17] = 1.0 / 32768.0;
        assert!(camplusplus_fbank(&pcm).unwrap()[0].iter().all(|x| *x == f32::EPSILON.ln()));
    }
}
