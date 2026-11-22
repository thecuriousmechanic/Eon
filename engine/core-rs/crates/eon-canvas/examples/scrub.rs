//! Read-only JPEG timeline: `cargo run -p eon-canvas --example scrub -- 1250 a.jpg 1480 b.jpg`.
//! The operator supplies monotonic timestamps and images. No capture service is opened.

use std::fs::File;
use std::io::{Cursor, Read};
use std::sync::Arc;

use eframe::egui;
use eon_primitives::{EonError, EonResult};
use image::codecs::jpeg::JpegDecoder;
use image::{DynamicImage, ImageDecoder, Limits};

const MAX_FRAMES: usize = 150;
const WINDOW_MS: u64 = 30_000;
const MAX_INPUT_BYTES: usize = 30 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;
const PREVIEW_EDGE: u32 = 256;

fn invalid(message: &str) -> EonError {
    EonError::ValidationError(message.into())
}

#[derive(Default)]
struct Timeline {
    frames: Vec<(u64, Arc<egui::ColorImage>)>,
    input_bytes: usize,
}

impl Timeline {
    fn push(&mut self, time: u64, input: impl Read) -> EonResult<()> {
        if self.frames.len() == MAX_FRAMES
            || self.frames.last().is_some_and(|(last, _)| time <= *last)
            || self
                .frames
                .first()
                .is_some_and(|(first, _)| time - first >= WINDOW_MS)
        {
            return Err(invalid(
                "expected up to 150 ordered frames within 30 seconds",
            ));
        }
        let remaining = MAX_INPUT_BYTES - self.input_bytes;
        let mut bytes = Vec::new();
        input
            .take(remaining as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid("cannot read frame"))?;
        if bytes.len() > remaining {
            return Err(invalid("timeline exceeds 30 MiB of compressed input"));
        }
        let mut decoder =
            JpegDecoder::new(Cursor::new(&bytes)).map_err(|_| invalid("invalid JPEG"))?;
        // max_alloc alone does not cover the JPEG backend's progressive coefficient
        // buffers. Budget padded 32x32 MCUs before decoding: four channels of i16
        // coefficients, expanded samples, RGB(A) output and scratch space.
        let (w, h) = decoder.dimensions();
        let padded_pixels = u64::from(w.next_multiple_of(32)) * u64::from(h.next_multiple_of(32));
        if padded_pixels * 24 > MAX_DECODE_BYTES {
            return Err(invalid("JPEG exceeds preview pixel budget"));
        }
        let mut limits = Limits::default();
        limits.max_image_width = Some(4096);
        limits.max_image_height = Some(4096);
        limits.max_alloc = Some(MAX_DECODE_BYTES);
        decoder
            .set_limits(limits)
            .map_err(|_| invalid("JPEG exceeds preview limits"))?;
        let image =
            DynamicImage::from_decoder(decoder).map_err(|_| invalid("cannot decode JPEG"))?;
        let image = if image.width().max(image.height()) > PREVIEW_EDGE {
            image.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE)
        } else {
            image
        }
        .into_rgb8();
        let size = [image.width() as usize, image.height() as usize];
        self.frames.push((
            time,
            Arc::new(egui::ColorImage::from_rgb(size, image.as_raw())),
        ));
        self.input_bytes += bytes.len();
        Ok(())
    }

    fn duration(&self) -> u64 {
        self.frames.last().unwrap().0 - self.frames[0].0
    }

    // At an unsampled instant, show the most recent preceding frame, not a future scene.
    fn at(&self, elapsed: u64) -> usize {
        let time = self.frames[0].0 + elapsed.min(self.duration());
        self.frames
            .partition_point(|(timestamp, _)| *timestamp <= time)
            - 1
    }
}

struct Review {
    timeline: Timeline,
    elapsed: u64,
    texture: Option<(usize, egui::TextureHandle)>,
}

impl eframe::App for Review {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        let duration = self.timeline.duration();
        ui.add(egui::Slider::new(&mut self.elapsed, 0..=duration).text("ms from start"));
        let index = self.timeline.at(self.elapsed);
        let (timestamp, image) = &self.timeline.frames[index];
        ui.label(format!(
            "Frame {} of {} at {timestamp} ms",
            index + 1,
            self.timeline.frames.len()
        ));
        if self.texture.as_ref().is_none_or(|(last, _)| *last != index) {
            self.texture = Some((
                index,
                ui.ctx().load_texture(
                    "selected frame",
                    Arc::clone(image),
                    egui::TextureOptions::LINEAR,
                ),
            ));
        }
        ui.image(&self.texture.as_ref().unwrap().1);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let mut timeline = Timeline::default();
    while let Some(time) = args.next() {
        let time = time
            .to_str()
            .and_then(|s| s.parse().ok())
            .ok_or("expected timestamp in milliseconds")?;
        let path = args.next().ok_or("missing JPEG path after timestamp")?;
        if !std::fs::metadata(&path)?.is_file() {
            return Err("expected a regular JPEG file".into());
        }
        timeline.push(time, File::open(path)?)?;
    }
    if timeline.frames.is_empty() {
        return Err(
            "usage: scrub <timestamp_ms> <frame.jpg> [<timestamp_ms> <frame.jpg> ...]".into(),
        );
    }
    Ok(eframe::run_native(
        "EON keyframe review",
        Default::default(),
        Box::new(move |_| {
            Ok(Box::new(Review {
                timeline,
                elapsed: 0,
                texture: None,
            }))
        }),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ExtendedColorType, codecs::jpeg::JpegEncoder};

    fn jpeg(width: u16, luma: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        JpegEncoder::new(&mut bytes)
            .encode(
                &vec![luma; usize::from(width)],
                u32::from(width),
                1,
                ExtendedColorType::L8,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn irregular_timestamps_scrub_to_real_pixels_even_near_clock_limit() {
        let mut timeline = Timeline::default();
        timeline
            .push(u64::MAX - 300, jpeg(512, 0).as_slice())
            .unwrap();
        timeline.push(u64::MAX, jpeg(1, 255).as_slice()).unwrap();
        for (elapsed, index, width, pixel) in [
            (0, 0, 256, egui::Color32::BLACK),
            (299, 0, 256, egui::Color32::BLACK),
            (300, 1, 1, egui::Color32::WHITE),
            (u64::MAX, 1, 1, egui::Color32::WHITE),
        ] {
            assert_eq!(timeline.at(elapsed), index);
            let image = &timeline.frames[index].1;
            assert_eq!(image.size, [width, 1]);
            assert_eq!(image.pixels[0], pixel);
        }
    }

    #[test]
    fn progressive_color_frame_is_rejected_by_geometry_before_decoding_scans() {
        let mut bytes = Vec::new();
        JpegEncoder::new(&mut bytes)
            .encode(&[0; 3], 1, 1, ExtendedColorType::Rgb8)
            .unwrap();
        let sof = bytes.windows(2).position(|m| m == [255, 192]).unwrap();
        bytes[sof + 1] = 194; // SOF2: progressive color, change the frame size
        bytes[sof + 5..sof + 9].copy_from_slice(&[16, 0, 16, 0]);
        let sos = bytes.windows(2).position(|m| m == [255, 218]).unwrap();
        let len = u16::from_be_bytes([bytes[sos + 2], bytes[sos + 3]]) as usize;
        bytes[sos + len] = 0; // first scan contains DC coefficients only
        let err = Timeline::default().push(0, bytes.as_slice()).unwrap_err();
        assert!(matches!(err, EonError::ValidationError(reason) if reason.contains("pixel budget")));
    }

    #[test]
    fn rejected_inputs_do_not_change_the_timeline() {
        let mut timeline = Timeline::default();
        let good = jpeg(1, 32);
        timeline.push(1, good.as_slice()).unwrap();
        for (time, bytes) in [
            (0, good.as_slice()),
            (1, &good),
            (30_001, &good),
            (2, &b"not an image"[..]),
            (2, &jpeg(4097, 0)),
        ] {
            assert!(timeline.push(time, bytes).is_err());
        }
        assert!(timeline.push(2, std::io::repeat(0)).is_err());
        assert_eq!(timeline.frames.len(), 1);
        assert_eq!(timeline.input_bytes, good.len());
        for time in 2..=MAX_FRAMES as u64 {
            timeline.push(time, good.as_slice()).unwrap();
        }
        assert!(timeline.push(200, good.as_slice()).is_err());
    }
}
