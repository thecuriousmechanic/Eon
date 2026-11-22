//! Opt-in, local PP-OCRv4 for **already authorized** JPEG pixels. Horizontal
//! text regions in raster coordinates, not accessibility selectors, widget
//! classifications or action targets. No capture, model downloads or subprocesses.

mod detection;

use std::time::{Duration, Instant};

use image::{GenericImageView, Rgb, RgbImage, imageops};
use ort::session::{RunOptions, Session};
use ort::value::Tensor;
use sha2::{Digest, Sha256};

pub use crate::PixelBounds;
use crate::still::{self, check_time, deadline, limited};
use crate::{EonError, EonResult};
const MAX_CANDIDATES: usize = 64;

// RECORD hashes in the PyPI-authenticated rapidocr-onnxruntime 1.4.4 wheel.
const DET_SHA256: &str = "d2a7720d45a54257208b1e13e36a8479894cb74155a5efe29462512d42f49da9";
const REC_SHA256: &str = "48fc40f24f6d2a207a2b1091d3437eb3cc3eb6b676dc3ef9c37384005483683b";

#[derive(Debug, Clone, PartialEq)]
pub struct OcrLine {
    /// Untrusted visible text. Not executable and not evidence of clickability.
    pub text: String,
    /// Mean greedy CTC probability of the emitted (nonblank) characters.
    pub confidence: f32,
    pub bounds: PixelBounds,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OcrPage {
    pub width: u32,
    pub height: u32,
    /// Top-to-bottom rows, left-to-right within each row. May be empty.
    pub lines: Vec<OcrLine>,
}

fn failed(reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: "PaddleOCR".into(),
        reason: reason.to_string(),
    }
}

/// Paddle v4 consumes BGR NCHW, normalized to [-1, 1]; the recognizer
/// expects right padding with zero **after** normalization.
fn input(
    image: &impl GenericImageView<Pixel = Rgb<u8>>,
    w: u32,
    h: u32,
    stride: u32,
) -> EonResult<Tensor<f32>> {
    let scaled = imageops::resize(image, w, h, imageops::FilterType::Triangle);
    let plane = (h * stride) as usize;
    let mut data = vec![0.0; 3 * plane];
    for (x, y, rgb) in scaled.enumerate_pixels() {
        let off = (y * stride + x) as usize;
        for ch in 0..3 {
            data[ch * plane + off] = f32::from(rgb[2 - ch]) / 127.5 - 1.0;
        }
    }
    Tensor::from_array(([1usize, 3, h as usize, stride as usize], data)).map_err(failed)
}

pub struct PaddleOcr {
    detector: Session,
    recognizer: Session,
    alphabet: Vec<String>,
}

impl PaddleOcr {
    /// Verify both graphs **before** passing either to the native runtime. The
    /// character dictionary is metadata of the pinned recognizer, not a local
    /// config, vocabulary file, plugin or network dependency.
    pub fn from_models(detector: &[u8], recognizer: &[u8]) -> EonResult<Self> {
        for (bytes, len, digest) in [
            (detector, 4_745_517, DET_SHA256),
            (recognizer, 10_857_958, REC_SHA256),
        ] {
            if bytes.len() != len || format!("{:x}", Sha256::digest(bytes)) != digest {
                return Err(EonError::ValidationError(
                    "PP-OCRv4 model digest mismatch".into(),
                ));
            }
        }
        let env = eon_primitives::onnx::environment()?;
        let load = |graph| {
            Session::builder(env)
                .map_err(failed)?
                .with_intra_threads(1)
                .map_err(failed)?
                .with_inter_threads(1)
                .map_err(failed)?
                // Caller-controlled crop sizes vary between frames. Don't keep
                // per-shape allocation plans or a CPU arena at its high-water mark.
                .with_memory_pattern(false)
                .map_err(failed)?
                .with_execution_providers([ort::ep::CPU::default()
                    .with_arena_allocator(false)
                    .build()])
                .map_err(failed)?
                .commit_from_memory(graph)
                .map_err(failed)
        };
        let detector = load(detector)?;
        let recognizer = load(recognizer)?;
        let dictionary = recognizer
            .metadata()
            .map_err(failed)?
            .custom("character")
            .ok_or_else(|| failed("missing PP-OCRv4 character metadata"))?;
        // Index zero is the CTC blank; space follows the embedded dictionary.
        let mut alphabet = vec![String::new()];
        alphabet.extend(dictionary.lines().map(str::to_owned));
        alphabet.push(" ".into());
        Ok(Self {
            detector,
            recognizer,
            alphabet,
        })
    }

    /// Ground one caller-supplied still. JPEG bytes are never interpreted as a
    /// filename. `budget` covers this frame, including all its recognizer runs.
    /// ORT termination is cooperative (at kernel boundaries), not a process kill.
    pub fn recognize_jpeg(&mut self, jpeg: &[u8], budget: Duration) -> EonResult<OcrPage> {
        self.recognize(jpeg, None, deadline(budget)?)
    }

    /// Ground only the sealed descriptor already selected by its capture owner.
    /// Does not query history, select a newer frame or hold a producer lock.
    #[cfg(target_os = "linux")]
    pub fn recognize_selected(
        &mut self,
        frame: &crate::VideoKeyframe<crate::shared::SharedJpeg>,
        budget: Duration,
    ) -> EonResult<OcrPage> {
        let end = deadline(budget)?;
        let jpeg = frame.jpeg_bytes.read_limited(still::MAX_JPEG)?;
        self.recognize(&jpeg, Some((frame.width, frame.height)), end)
    }

    fn recognize(
        &mut self,
        bytes: &[u8],
        expected: Option<(u32, u32)>,
        end: Instant,
    ) -> EonResult<OcrPage> {
        still::run(bytes, expected, end, |image, opts| {
            self.infer(image, opts, end)
        })
    }

    fn infer(&mut self, image: &RgbImage, opts: &RunOptions, end: Instant) -> EonResult<OcrPage> {
        let (width, height) = image.dimensions();
        let scale = (960.0 / width.max(height) as f32).min(1.0);
        let aligned = |side| (((side as f32 * scale / 32.0).round() as u32).max(1)) * 32;
        let (dw, dh) = (aligned(width), aligned(height));
        let regions = {
            let tensor = input(image, dw, dh, dw)?;
            check_time(end)?;
            let out = self
                .detector
                .run_with_options(ort::inputs![tensor], opts)
                .map_err(failed)?;
            let (shape, scores) = out[0].try_extract_tensor::<f32>().map_err(failed)?;
            if **shape != [1, 1, i64::from(dh), i64::from(dw)] {
                return Err(failed("unexpected text probability map dimensions"));
            }
            detection::regions(scores, dw, dh, width, height)?
        };
        let mut lines = Vec::new();
        for bounds in regions {
            check_time(end)?;
            let (w, h) = (bounds.right - bounds.left, bounds.bottom - bounds.top);
            let crop = image.view(bounds.left, bounds.top, w, h);
            let rw = (48 * w).div_ceil(h).clamp(1, 960);
            let tensor = input(&*crop, rw, 48, rw.max(320))?;
            let out = self
                .recognizer
                .run_with_options(ort::inputs![tensor], opts)
                .map_err(failed)?;
            let (shape, logits) = out[0].try_extract_tensor::<f32>().map_err(failed)?;
            if shape.len() != 3 || shape[0] != 1 || shape[2] != self.alphabet.len() as i64 {
                return Err(failed(
                    "recognizer shape disagrees with character dictionary",
                ));
            }
            if let Some((text, confidence)) = ctc(logits, &self.alphabet) {
                lines.push(OcrLine {
                    text,
                    confidence,
                    bounds,
                });
            }
        }
        Ok(OcrPage {
            width,
            height,
            lines,
        })
    }
}

fn ctc(scores: &[f32], alphabet: &[String]) -> Option<(String, f32)> {
    let mut text = String::new();
    let (mut prev, mut count, mut total) = (0, 0u32, 0.0);
    for step in scores.chunks_exact(alphabet.len()) {
        let (index, &p) = step.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
        if index != 0 && prev != index {
            text.push_str(&alphabet[index]);
            total += p;
            count += 1;
        }
        prev = index;
    }
    let confidence = total / count.max(1) as f32;
    let text = text.trim();
    (!text.is_empty() && (0.6..=1.0).contains(&confidence)).then(|| (text.to_owned(), confidence))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctc_collapses_repeats_but_not_across_blank_and_drops_low_confidence() {
        let alphabet = ["", "A", "B"].map(String::from);
        assert_eq!(
            ctc(&[0., 1., 0., 0., 1., 0., 1., 0., 0., 0., 1., 0.], &alphabet),
            Some(("AA".into(), 1.0))
        );
        assert!(ctc(&[0.3, 0.5, 0.2], &alphabet).is_none());
    }
}
