//! Offline speaker embeddings and the enrolled-owner cosine gate. The encoder
//! version identifies both the **exact weights** and the **feature pipeline**.
//! Only embeddings from the same version are comparable.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use eon_primitives::{EonError, EonResult};
use ort::session::Session;
use ort::value::Tensor;
use sha2::{Digest, Sha256};

use crate::features::{NUM_MEL_BINS, camplusplus_fbank, wespeaker_fbank};

pub(crate) mod windows;
mod agreement;
pub use agreement::{SpeakerAgreement, SpeakerPolicy};

/// Dimension of the original WeSpeaker ResNet34-LM embedding.
pub const EMBEDDING_DIM: usize = 256;
const MIN_FRAMES: usize = 50;
/// A complete 25 ms window, plus 49 shifts of 10 ms (515 ms at 16 kHz).
pub(crate) const MIN_SAMPLES: usize = 400 + (MIN_FRAMES - 1) * 160;

fn speaker_err(operation: &str, reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: format!("speaker.{operation}"),
        reason: reason.to_string(),
    }
}

/// Persist this version alongside any saved vector, not merely its length or a
/// filename. Changing either the weights or the front end requires a new version.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SpeakerEncoder {
    /// The PCM16/Hamming/utterance-CMN pipeline. Not the older ASR-feature vectors.
    WeSpeakerResNet34LmV1,
    /// The bilingual 3D-Speaker CAM++ advanced graph, normalized PCM/Povey/utterance CMN.
    CamPlusPlusZhEnAdvancedV1,
}

impl SpeakerEncoder {
    /// Downloader artifact identifier, not an enrollment format.
    pub const fn model_id(self) -> &'static str {
        match self {
            Self::WeSpeakerResNet34LmV1 => "wespeaker-resnet34",
            Self::CamPlusPlusZhEnAdvancedV1 => "camplusplus-zh-en-advanced",
        }
    }

    pub const fn dimension(self) -> usize {
        match self {
            Self::WeSpeakerResNet34LmV1 => EMBEDDING_DIM,
            Self::CamPlusPlusZhEnAdvancedV1 => 192,
        }
    }

    // These pins are part of the identity of existing enrollments. Do not
    // reinterpret them when adding new model releases to the downloader.
    fn pin(self) -> (u64, &'static str) {
        match self {
            Self::WeSpeakerResNet34LmV1 => (
                26_530_309,
                "7bb2f06e9df17cdf1ef14ee8a15ab08ed28e8d0ef5054ee135741560df2ec068",
            ),
            Self::CamPlusPlusZhEnAdvancedV1 => (
                28_281_165,
                "53cab20ba8f2eb2f6e2d89ecc18e88ef3222169b61b4419cbc13666744a4574b",
            ),
        }
    }
}

pub struct SpeakerEmbedder {
    session: Session,
    encoder: SpeakerEncoder,
}

impl SpeakerEmbedder {
    /// Load the original WeSpeaker pipeline. Does not select a model by filename,
    /// tensor dimensions, or availability of an alternative.
    pub fn load(model: impl AsRef<Path>) -> EonResult<Self> {
        Self::load_for(SpeakerEncoder::WeSpeakerResNet34LmV1, model)
    }

    pub fn load_for(encoder: SpeakerEncoder, model: impl AsRef<Path>) -> EonResult<Self> {
        let (size, digest) = encoder.pin();
        let file = File::open(model).map_err(|e| speaker_err("load", e))?;
        let metadata = file.metadata().map_err(|e| speaker_err("load", e))?;
        if !metadata.is_file() || metadata.len() != size {
            return Err(speaker_err("load", format!("wrong file type or length for {encoder:?}")));
        }
        let mut bytes = Vec::with_capacity(size as usize);
        file.take(size + 1).read_to_end(&mut bytes).map_err(|e| speaker_err("load", e))?;
        if bytes.len() as u64 != size || format!("{:x}", Sha256::digest(&bytes)) != digest {
            return Err(speaker_err("load", format!("model digest mismatch for {encoder:?}")));
        }
        // Give ORT the exact verified bytes, not a path that might have changed.
        let session = Session::builder(crate::runtime::environment()?)
            .map_err(|e| speaker_err("builder", e))?
            .with_intra_threads(2)
            .map_err(|e| speaker_err("threads", e))?
            .commit_from_memory(&bytes)
            .map_err(|e| speaker_err("load", e))?;
        Ok(Self { session, encoder })
    }

    pub fn encoder(&self) -> SpeakerEncoder {
        self.encoder
    }

    /// Embeds only this slice of normalized 16 kHz mono speech (at least 515 ms).
    pub fn embed(&mut self, samples: &[f32]) -> EonResult<SpeakerEmbedding> {
        let frames = match self.encoder {
            SpeakerEncoder::WeSpeakerResNet34LmV1 => wespeaker_fbank(samples)?,
            SpeakerEncoder::CamPlusPlusZhEnAdvancedV1 => camplusplus_fbank(samples)?,
        };
        let t = frames.len();
        if t < MIN_FRAMES {
            return Err(EonError::ValidationError(format!(
                "speaker embedding needs at least {MIN_FRAMES} frames, got {t}"
            )));
        }
        let mut mean = [0.0f32; NUM_MEL_BINS];
        for frame in &frames {
            for (m, x) in mean.iter_mut().zip(frame) {
                *m += x;
            }
        }
        mean.iter_mut().for_each(|m| *m /= t as f32);
        let flat: Vec<f32> = frames
            .iter()
            .flat_map(|frame| frame.iter().zip(&mean).map(|(x, m)| x - m))
            .collect();

        let input = Tensor::from_array(([1usize, t, NUM_MEL_BINS], flat)).map_err(|e| speaker_err("input", e))?;
        // Both pinned graphs have a single input (features) and output (embedding).
        let outputs = self.session.run(ort::inputs![input]).map_err(|e| speaker_err("run", e))?;
        let (_, embedding) = outputs[0].try_extract_tensor::<f32>().map_err(|e| speaker_err("embs", e))?;
        SpeakerEmbedding::from_values(self.encoder, embedding)
    }
}

/// Encoder-bound, unit-length voiceprint. No Debug output of biometric data.
#[derive(Clone)]
pub struct SpeakerEmbedding {
    encoder: SpeakerEncoder,
    values: Vec<f32>,
}

impl SpeakerEmbedding {
    /// For loading a vector with a **known** originating model and front end.
    /// If the origin of a persisted vector is unknown, require re-enrollment.
    pub fn from_values(encoder: SpeakerEncoder, values: &[f32]) -> EonResult<Self> {
        let norm = magnitude(values);
        if values.len() != encoder.dimension() || !norm.is_finite() || norm == 0.0 {
            return Err(EonError::ValidationError(format!(
                "expected a finite nonzero {}-dimensional vector from {encoder:?}",
                encoder.dimension()
            )));
        }
        Ok(Self {
            encoder,
            values: values.iter().map(|&v| (f64::from(v) / norm) as f32).collect(),
        })
    }

    pub fn encoder(&self) -> SpeakerEncoder {
        self.encoder
    }

    pub fn as_slice(&self) -> &[f32] {
        &self.values
    }
}

fn magnitude(v: &[f32]) -> f64 {
    v.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>().sqrt()
}

pub fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = magnitude(v);
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|&x| (f64::from(x) / norm) as f32).collect()
}

/// Raw cosine for diagnostics; the owner gate also checks encoder identity.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let norm = magnitude(a) * magnitude(b);
    if norm == 0.0 {
        return 0.0;
    }
    let dot: f64 = a.iter().zip(b).map(|(&x, &y)| f64::from(x) * f64::from(y)).sum();
    (dot / norm).clamp(-1.0, 1.0) as f32
}

/// Accepts only comparable, current candidate evidence above the configured threshold.
#[derive(Clone)]
pub struct SpeakerGate {
    owner: SpeakerEmbedding,
    threshold: f32,
}

impl SpeakerGate {
    pub fn enroll(owner: &SpeakerEmbedding, threshold: f32) -> EonResult<Self> {
        if !(-1.0..=1.0).contains(&threshold) {
            return Err(EonError::ValidationError("expected a cosine threshold in [-1, 1]".into()));
        }
        Ok(Self { owner: owner.clone(), threshold })
    }

    pub fn encoder(&self) -> SpeakerEncoder {
        self.owner.encoder
    }

    pub fn score(&self, candidate: &SpeakerEmbedding) -> Option<f32> {
        (self.owner.encoder == candidate.encoder).then(|| cosine(self.owner.as_slice(), candidate.as_slice()))
    }

    pub fn accepts(&self, candidate: &SpeakerEmbedding) -> bool {
        self.score(candidate).is_some_and(|score| score >= self.threshold)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SpeakerEncoder::*;

    fn unit(encoder: SpeakerEncoder, index: usize) -> SpeakerEmbedding {
        let mut v = vec![0.0; encoder.dimension()];
        v[index] = 1.0;
        SpeakerEmbedding::from_values(encoder, &v).unwrap()
    }

    #[test]
    fn cosine_of_a_vector_with_itself_is_one_and_orthogonal_is_zero() {
        let a = normalize(&[3.0, 4.0, 0.0]);
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn enrollment_and_gate_are_encoder_bound() {
        for encoder in [WeSpeakerResNet34LmV1, CamPlusPlusZhEnAdvancedV1] {
            let owner = unit(encoder, 0);
            let gate = SpeakerGate::enroll(&owner, 0.35).unwrap();
            assert!(gate.accepts(&owner));
            assert!(!gate.accepts(&unit(encoder, 1)));
            assert!(SpeakerEmbedding::from_values(encoder, &vec![f32::NAN; encoder.dimension()]).is_err());
            assert!(SpeakerEmbedding::from_values(encoder, &vec![0.0; encoder.dimension()]).is_err());
            assert!(SpeakerEmbedding::from_values(encoder, &[1.0; 128]).is_err());
            assert!(SpeakerGate::enroll(&owner, f32::NAN).is_err());
        }
        let gate = SpeakerGate::enroll(&unit(WeSpeakerResNet34LmV1, 0), -1.0).unwrap();
        assert_eq!(gate.score(&unit(CamPlusPlusZhEnAdvancedV1, 0)), None);
        assert!(!gate.accepts(&unit(CamPlusPlusZhEnAdvancedV1, 0)));
        assert!(SpeakerEmbedding::from_values(CamPlusPlusZhEnAdvancedV1, &[1.0; EMBEDDING_DIM]).is_err());
    }

    #[test]
    fn versioned_model_pins_match_the_downloader() {
        let lock: serde_json::Value =
            serde_json::from_str(include_str!("../../../../../scripts/models.lock.json")).unwrap();
        for encoder in [WeSpeakerResNet34LmV1, CamPlusPlusZhEnAdvancedV1] {
            let model = lock["models"].as_array().unwrap().iter().find(|model| model["id"] == encoder.model_id()).unwrap();
            let (size, sha256) = encoder.pin();
            assert_eq!(model["sha256"], sha256);
            assert_eq!(model["size_bytes"], size);
        }
    }
}
