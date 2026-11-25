use std::io::Cursor;
#[cfg(any(feature = "paddle-ocr", feature = "omniparser-icons"))]
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbImage};
#[cfg(any(feature = "paddle-ocr", feature = "omniparser-icons"))]
use ort::session::RunOptions;

use crate::{EonError, EonResult};

pub(crate) const MAX_JPEG: usize = 8 * 1024 * 1024;
const MAX_SIDE: u32 = 4096;
const MAX_PIXELS: u64 = 4 * 1024 * 1024;
const MAX_TIME: Duration = Duration::from_secs(10);

fn failed(reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: "selected still".into(),
        reason: reason.to_string(),
    }
}

pub(crate) fn limited(resource: &str, requested: u64, available: u64) -> EonError {
    EonError::ResourceExhausted {
        resource: resource.into(),
        requested,
        available,
    }
}

pub(crate) fn deadline(budget: Duration) -> EonResult<Instant> {
    if budget.is_zero() || budget > MAX_TIME {
        return Err(EonError::ValidationError(
            "still inference budget must be in (0, 10s]".into(),
        ));
    }
    Ok(Instant::now() + budget)
}

pub(crate) fn check_time(end: Instant) -> EonResult<()> {
    if Instant::now() >= end {
        Err(failed("frame deadline exceeded"))
    } else {
        Ok(())
    }
}

pub(crate) fn decode(bytes: &[u8], expected: Option<(u32, u32)>) -> EonResult<RgbImage> {
    if bytes.len() > MAX_JPEG {
        return Err(limited("JPEG bytes", bytes.len() as u64, MAX_JPEG as u64));
    }
    if !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return Err(EonError::ValidationError(
            "expected one complete JPEG".into(),
        ));
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(64 * 1024 * 1024);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Jpeg);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(failed)?;
    let (w, h) = decoder.dimensions();
    let pixels = u64::from(w) * u64::from(h);
    if pixels > MAX_PIXELS {
        return Err(limited("still pixels", pixels, MAX_PIXELS));
    }
    if w == 0 || h == 0 || expected.is_some_and(|dims| dims != (w, h)) {
        return Err(EonError::ValidationError(
            "invalid or mismatched frame dimensions".into(),
        ));
    }
    Ok(DynamicImage::from_decoder(decoder)
        .map_err(failed)?
        .into_rgb8())
}

/// One budget covers reading (if selected), decoding, preprocessing, all native
/// runs and postprocessing. ORT termination is cooperative at kernel boundaries.
#[cfg(any(feature = "paddle-ocr", feature = "omniparser-icons"))]
pub(crate) fn run<T>(
    bytes: &[u8],
    expected: Option<(u32, u32)>,
    end: Instant,
    infer: impl FnOnce(&RgbImage, &RunOptions) -> EonResult<T>,
) -> EonResult<T> {
    check_time(end)?;
    let image = decode(bytes, expected)?;
    check_time(end)?;
    let options = RunOptions::new().map_err(failed)?;
    let result = std::thread::scope(|scope| {
        let (done, rx) = channel::<()>();
        let opts = &options;
        std::thread::Builder::new()
            .name("eon-still-deadline".into())
            .spawn_scoped(scope, move || {
                if rx.recv_timeout(end.saturating_duration_since(Instant::now()))
                    == Err(RecvTimeoutError::Timeout)
                {
                    let _ = opts.terminate();
                }
            })
            .map_err(failed)?;
        let result = infer(&image, &options);
        drop(done);
        result
    });
    check_time(end)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_oversized_pixels_from_the_jpeg_header_before_decompression() {
        let mut bytes = include_bytes!("../tests/fixtures/settings.jpg").to_vec();
        let sof = bytes.windows(2).position(|m| m == [0xff, 0xc0]).unwrap();
        // SOF0 length, precision, then two big-endian u16 raster dimensions.
        bytes[sof + 5..sof + 9].copy_from_slice(&[0x0c, 0x00, 0x0c, 0x00]);
        assert!(matches!(
            decode(&bytes, None),
            Err(EonError::ResourceExhausted { .. })
        ));
        bytes[sof + 5..sof + 9].copy_from_slice(&[0x00, 0x08, 0xff, 0xff]);
        assert!(decode(&bytes, None).is_err());
    }

    #[test]
    fn non_image_input_and_excess_bytes_are_not_processed() {
        assert!(decode(b"file:///etc/hosts", None).is_err());
        assert!(decode(&[0xff, 0xd8, 0xff, 0xd9], None).is_err());
        assert!(matches!(
            decode(&vec![0; MAX_JPEG + 1], None),
            Err(EonError::ResourceExhausted { .. })
        ));
        assert!(deadline(Duration::ZERO).is_err());
        assert!(deadline(MAX_TIME + Duration::from_millis(1)).is_err());
    }
}
