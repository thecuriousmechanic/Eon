//! Local OmniParser-v2 icon proposals for one explicitly supplied still. The
//! single-class YOLO head does not read text, name icons or establish clickability.

use std::time::{Duration, Instant};

use image::{RgbImage, imageops};
use ort::session::Session;
use ort::value::Tensor;
use sha2::{Digest, Sha256};

use crate::still::{self, check_time, deadline};
use crate::{EonError, EonResult, PixelBounds};

#[cfg(all(
    feature = "omniparser-openvino",
    target_os = "linux",
    target_arch = "x86_64"
))]
mod openvino_cpu;

const MODEL_BYTES: usize = 80_305_670;
const MODEL_SHA256: &str = "c0d3d261e3368a5e51fe0d632c5d62e4bd4da489ac4b95fbbee450eb640e36fe";
const SIDE: u32 = 640;
const ANCHORS: usize = 8400;
const MIN_CONFIDENCE: f32 = 0.25;
const NMS_IOU: f32 = 0.45;
const MAX_ICONS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IconDetection {
    pub bounds: PixelBounds,
    /// The pinned model's single `icon` class score, not a probability of actionability.
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IconPage {
    pub width: u32,
    pub height: u32,
    /// Descending confidence after single-class non-maximum suppression.
    pub icons: Vec<IconDetection>,
}

fn failed(reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: "OmniParser icon detection".into(),
        reason: reason.to_string(),
    }
}

/// One verified ONNX graph and a reusable local CPU backend. No pickle,
/// external model data, remote model code or network lookup.
pub struct IconDetector {
    backend: Backend,
}

enum Backend {
    Ort(Session),
    #[cfg(all(
        feature = "omniparser-openvino",
        target_os = "linux",
        target_arch = "x86_64"
    ))]
    OpenVino(openvino_cpu::CpuDetector),
}

impl IconDetector {
    /// Accepts only the `omniparser-v2-icon-detect-onnx` artifact from the model lock.
    /// ORT is the default. `EON_ICON_BACKEND=openvino-cpu` explicitly requires the
    /// opt-in `omniparser-openvino` feature and the pinned native CPU runtime.
    pub fn from_model(graph: &[u8]) -> EonResult<Self> {
        if graph.len() != MODEL_BYTES || format!("{:x}", Sha256::digest(graph)) != MODEL_SHA256 {
            return Err(EonError::ValidationError(
                "icon model digest mismatch".into(),
            ));
        }
        match std::env::var("EON_ICON_BACKEND").as_deref() {
            Ok("ort-cpu") | Err(std::env::VarError::NotPresent) => {}
            #[cfg(all(
                feature = "omniparser-openvino",
                target_os = "linux",
                target_arch = "x86_64"
            ))]
            Ok("openvino-cpu") => {
                return Ok(Self {
                    backend: Backend::OpenVino(openvino_cpu::CpuDetector::load(graph)?),
                });
            }
            _ => return Err(failed("unsupported EON_ICON_BACKEND")),
        }
        let session = Session::builder(eon_primitives::onnx::environment()?)
            .map_err(failed)?
            .with_intra_threads(8)
            .map_err(failed)?
            .with_inter_threads(1)
            .map_err(failed)?
            .with_execution_providers([ort::ep::CPU::default().build()])
            .map_err(failed)?
            .commit_from_memory(graph)
            .map_err(failed)?;
        Ok(Self {
            backend: Backend::Ort(session),
        })
    }

    /// The backend actually constructed, for diagnostics.
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            Backend::Ort(_) => "ORT CPU (8 intra-op threads)",
            #[cfg(all(
                feature = "omniparser-openvino",
                target_os = "linux",
                target_arch = "x86_64"
            ))]
            Backend::OpenVino(_) => "OpenVINO CPU (BF16, 1 stream, up to 32 threads)",
        }
    }

    /// Detect icons on these JPEG bytes (stored orientation). The budget is for
    /// the complete call, not just the native forward pass; no partial results.
    pub fn detect_jpeg(&mut self, jpeg: &[u8], budget: Duration) -> EonResult<IconPage> {
        self.detect(jpeg, None, deadline(budget)?)
    }

    /// Read only the caller's selected sealed JPEG. No access to its history or
    /// producer lock, even if a newer frame exists.
    #[cfg(target_os = "linux")]
    pub fn detect_selected(
        &mut self,
        frame: &crate::VideoKeyframe<crate::shared::SharedJpeg>,
        budget: Duration,
    ) -> EonResult<IconPage> {
        let end = deadline(budget)?;
        let bytes = frame.jpeg_bytes.read_limited(still::MAX_JPEG)?;
        self.detect(&bytes, Some((frame.width, frame.height)), end)
    }

    fn detect(
        &mut self,
        bytes: &[u8],
        expected: Option<(u32, u32)>,
        end: Instant,
    ) -> EonResult<IconPage> {
        let session = match &mut self.backend {
            Backend::Ort(session) => session,
            #[cfg(all(
                feature = "omniparser-openvino",
                target_os = "linux",
                target_arch = "x86_64"
            ))]
            Backend::OpenVino(cpu) => {
                check_time(end)?;
                let image = still::decode(bytes, expected)?;
                check_time(end)?;
                let page = cpu.detect(&image, end)?;
                check_time(end)?;
                return Ok(page);
            }
        };
        still::run(bytes, expected, end, |image, opts| {
            let fit = Letterbox::new(image.width(), image.height());
            let mut data = vec![0.0; 3 * SIDE as usize * SIDE as usize];
            fit.fill_tensor(image, &mut data);
            let input = Tensor::from_array(([1usize, 3, SIDE as usize, SIDE as usize], data))
                .map_err(failed)?;
            check_time(end)?;
            let out = session
                .run_with_options(ort::inputs![input], opts)
                .map_err(failed)?;
            let (shape, channels) = out["output0"].try_extract_tensor::<f32>().map_err(failed)?;
            if **shape != [1, 5, ANCHORS as i64] {
                return Err(failed("unexpected YOLO11 head dimensions"));
            }
            Ok(IconPage {
                width: image.width(),
                height: image.height(),
                icons: fit.detections(channels)?,
            })
        })
    }
}

struct Letterbox {
    width: u32,
    height: u32,
    w: u32,
    h: u32,
    pad_x: u32,
    pad_y: u32,
}

impl Letterbox {
    fn new(width: u32, height: u32) -> Self {
        let long = width.max(height);
        let scaled = |v: u32| ((v * SIDE + long / 2) / long).max(1);
        let (w, h) = (scaled(width), scaled(height));
        Self {
            width,
            height,
            w,
            h,
            pad_x: (SIDE - w) / 2,
            pad_y: (SIDE - h) / 2,
        }
    }

    fn fill_tensor(&self, image: &RgbImage, data: &mut [f32]) {
        let resized = imageops::resize(image, self.w, self.h, imageops::FilterType::Triangle);
        let plane = (SIDE * SIDE) as usize;
        data.fill(114.0 / 255.0);
        for (x, y, rgb) in resized.enumerate_pixels() {
            let off = ((self.pad_y + y) * SIDE + self.pad_x + x) as usize;
            for ch in 0..3 {
                data[ch * plane + off] = f32::from(rgb[ch]) / 255.0;
            }
        }
    }

    fn detections(&self, channels: &[f32]) -> EonResult<Vec<IconDetection>> {
        // YOLO11 export has four center/size planes (model pixels) and a single
        // class probability plane. There is no separate objectness multiplier.
        let mut candidates = Vec::new();
        for i in 0..ANCHORS {
            let confidence = channels[4 * ANCHORS + i];
            if !(0.0..=1.0).contains(&confidence) {
                return Err(failed("invalid icon confidence"));
            }
            if confidence < MIN_CONFIDENCE {
                continue;
            }
            let [cx, cy, w, h] = std::array::from_fn(|ch| channels[ch * ANCHORS + i]);
            if ![cx, cy, w, h].iter().all(|v| v.is_finite()) || w < 0.0 || h < 0.0 {
                return Err(failed("invalid icon geometry"));
            }
            let x = |v: f32| (v - self.pad_x as f32).clamp(0.0, self.w as f32);
            let y = |v: f32| (v - self.pad_y as f32).clamp(0.0, self.h as f32);
            let edges = [
                x(cx - w * 0.5),
                y(cy - h * 0.5),
                x(cx + w * 0.5),
                y(cy + h * 0.5),
            ];
            if edges[0] < edges[2] && edges[1] < edges[3] {
                candidates.push((confidence, edges));
            }
        }
        candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut kept = Vec::new();
        for (score, rect) in candidates {
            if kept.iter().any(|&(_, other)| iou(rect, other) > NMS_IOU) {
                continue;
            }
            if kept.len() == MAX_ICONS {
                return Err(still::limited(
                    "icons per still",
                    (MAX_ICONS + 1) as u64,
                    MAX_ICONS as u64,
                ));
            }
            kept.push((score, rect));
        }
        let x = |v| v * self.width as f64 / self.w as f64;
        let y = |v| v * self.height as f64 / self.h as f64;
        Ok(kept
            .into_iter()
            .map(|(confidence, rect)| {
                let [l, t, r, b] = rect.map(f64::from);
                IconDetection {
                    bounds: PixelBounds {
                        left: x(l).floor() as u32,
                        top: y(t).floor() as u32,
                        right: x(r).ceil() as u32,
                        bottom: y(b).ceil() as u32,
                    },
                    confidence,
                }
            })
            .collect())
    }
}

fn iou(a: [f32; 4], b: [f32; 4]) -> f32 {
    let area = |r: [f32; 4]| (r[2] - r[0]) * (r[3] - r[1]);
    let overlap =
        (a[2].min(b[2]) - a[0].max(b[0])).max(0.0) * (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    overlap / (area(a) + area(b) - overlap)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Arithmetic tests for the five channel planes, separate from the real
    // model acceptance in tests/still_icons.rs.
    fn anchor(head: &mut [f32], i: usize, values: [f32; 5]) {
        for (ch, value) in values.into_iter().enumerate() {
            head[ch * ANCHORS + i] = value;
        }
    }

    #[test]
    fn undo_odd_portrait_padding_clip_and_suppress_in_float_coordinates() {
        let fit = Letterbox::new(200, 805);
        assert_eq!((fit.w, fit.h, fit.pad_x), (159, 640, 240));
        let mut head = vec![0.0; 5 * ANCHORS];
        anchor(&mut head, 0, [320.0, 320.0, 400.0, 800.0, 0.8]);
        anchor(&mut head, 1, [320.0, 320.0, 405.0, 810.0, 0.9]); // duplicate
        anchor(&mut head, 2, [100.0, 320.0, 40.0, 300.0, 0.99]); // only padding
        anchor(&mut head, 3, [359.25, 480.0, 79.5, 320.0, 0.7]); // bottom right quarter
        let icons = fit.detections(&head).unwrap();
        assert_eq!(icons.len(), 2);
        assert_eq!(icons[0].confidence, 0.9);
        assert_eq!(
            icons[0].bounds,
            PixelBounds {
                left: 0,
                top: 0,
                right: 200,
                bottom: 805
            }
        );
        assert_eq!(
            icons[1].bounds,
            PixelBounds {
                left: 100,
                top: 402,
                right: 200,
                bottom: 805
            }
        );
    }

    #[test]
    fn invalid_or_excessive_proposals_do_not_return_a_partial_page() {
        let fit = Letterbox::new(640, 640);
        let mut head = vec![0.0; 5 * ANCHORS];
        for values in [
            [10.0, 10.0, 5.0, 5.0, f32::NAN],
            [f32::INFINITY, 10.0, 5.0, 5.0, 0.8],
            [10.0, 10.0, -2.0, 5.0, 0.8],
        ] {
            anchor(&mut head, 0, values);
            assert!(fit.detections(&head).is_err());
        }
        for i in 0..=MAX_ICONS {
            anchor(&mut head, i, [(2 * i + 1) as f32, 1.0, 1.0, 1.0, 0.8]);
        }
        assert!(matches!(
            fit.detections(&head),
            Err(EonError::ResourceExhausted { .. })
        ));
    }
}
