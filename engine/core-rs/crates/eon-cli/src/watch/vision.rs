//! A single still selected at startup. No directory enumeration, capture, or reload.

mod graphics;
pub(super) use graphics::Painter;

use std::{fmt, io::Cursor, path::Path};

use image::{DynamicImage, ImageDecoder, Limits, RgbImage, codecs::png::PngDecoder};
use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_SIDE: u32 = 4096;
const MAX_PIXELS: u64 = 2 * 1024 * 1024;
const PREVIEW_W: u32 = 512;
const PREVIEW_H: u32 = 384;

#[derive(Default)]
pub(super) enum Selection {
    #[default]
    None,
    Frame(DynamicImage),
    Released,
}

impl fmt::Debug for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.status())
    }
}

impl Selection {
    pub(super) fn load(path: &Path) -> Result<Self, &'static str> {
        let bytes = read_regular(path)?;
        Ok(Self::Frame(decode(&bytes)?))
    }

    pub(super) fn release(&mut self) {
        if matches!(self, Self::Frame(_)) {
            *self = Self::Released;
        }
    }

    pub(super) fn status(&self) -> &'static str {
        match self {
            Self::None => {
                "No vision frame source. Select one still with watch <journal-dir> --image <file>."
            }
            Self::Frame(_) => "Selected local still. Leaving /vision releases it.",
            Self::Released => "Selected still released. Start watch again to select another.",
        }
    }
}

#[cfg(unix)]
fn read_regular(path: &Path) -> Result<Vec<u8>, &'static str> {
    use rustix::fs::{Mode, OFlags, open};
    use std::{fs, io::Read, os::unix::fs::MetadataExt};

    const FILE_ERROR: &str = "expected a readable, regular image file (not a link)";
    let selected = fs::symlink_metadata(path).map_err(|_| FILE_ERROR)?;
    if !selected.is_file() {
        return Err(FILE_ERROR);
    }
    // Re-check the opened object. A swap to a FIFO between stat and open must not hang.
    let flags =
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::CLOEXEC;
    let file = fs::File::from(open(path, flags, Mode::empty()).map_err(|_| FILE_ERROR)?);
    let meta = file.metadata().map_err(|_| FILE_ERROR)?;
    if !meta.is_file()
        || meta.len() == 0
        || (meta.dev(), meta.ino()) != (selected.dev(), selected.ino())
    {
        return Err(FILE_ERROR);
    }
    if meta.len() > MAX_BYTES as u64 {
        return Err("selected image exceeds the byte limit");
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    (&file)
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| FILE_ERROR)?;
    if bytes.len() > MAX_BYTES {
        return Err("selected image exceeds the byte limit");
    }
    let after = file.metadata().map_err(|_| FILE_ERROR)?;
    if bytes.len() as u64 != meta.len()
        || (meta.ctime(), meta.ctime_nsec()) != (after.ctime(), after.ctime_nsec())
    {
        return Err("selected image changed while reading");
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_regular(_: &Path) -> Result<Vec<u8>, &'static str> {
    Err("selecting a regular image file is not supported on this platform")
}

fn check_size(w: u32, h: u32) -> Result<(), &'static str> {
    // JPEG's progressive decoder retains i16 DCT coefficients for every component.
    // Bound the *padded* raster before entropy decoding, not just output RGB bytes.
    // The decoder accepts at most four components and a 4x4 (32px) MCU.
    if w == 0
        || h == 0
        || w > MAX_SIDE
        || h > MAX_SIDE
        || u64::from(w.div_ceil(32)) * u64::from(h.div_ceil(32)) * 1024 > MAX_PIXELS
    {
        Err("selected image exceeds the raster limit")
    } else {
        Ok(())
    }
}

fn decode(bytes: &[u8]) -> Result<DynamicImage, &'static str> {
    const INVALID: &str = "expected one supported, bounded PNG or JPEG still";
    if bytes.len() > MAX_BYTES {
        return Err("selected image exceeds the byte limit");
    }
    let image = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let mut limits = Limits::default();
        limits.max_image_width = Some(MAX_SIDE);
        limits.max_image_height = Some(MAX_SIDE);
        limits.max_alloc = Some(16 * 1024 * 1024);
        let decoder = PngDecoder::with_limits(Cursor::new(bytes), limits).map_err(|_| INVALID)?;
        let (w, h) = decoder.dimensions();
        check_size(w, h)?;
        if decoder.is_apng().map_err(|_| INVALID)? {
            return Err(INVALID);
        }
        let mut rgba = DynamicImage::from_decoder(decoder)
            .map_err(|_| INVALID)?
            .into_rgba8();
        // Flatten even fully transparent pixels before any protocol can receive their RGB.
        for pixel in rgba.pixels_mut() {
            let alpha = u16::from(pixel[3]);
            for c in &mut pixel.0[..3] {
                *c = ((u16::from(*c) * alpha + 127) / 255) as u8;
            }
            pixel[3] = 255;
        }
        DynamicImage::ImageRgba8(rgba)
    } else if bytes.starts_with(&[0xff, 0xd8]) && bytes.ends_with(&[0xff, 0xd9]) {
        let options = DecoderOptions::default()
            .set_strict_mode(true)
            .set_max_width(MAX_SIDE as usize)
            .set_max_height(MAX_SIDE as usize)
            .jpeg_set_max_scans(16)
            .jpeg_set_out_colorspace(ColorSpace::RGB);
        let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
        decoder.decode_headers().map_err(|_| INVALID)?;
        let (w, h) = decoder.dimensions().ok_or(INVALID)?;
        check_size(w as u32, h as u32)?;
        let rgb = decoder.decode().map_err(|_| INVALID)?;
        DynamicImage::ImageRgb8(RgbImage::from_raw(w as u32, h as u32, rgb).ok_or(INVALID)?)
    } else {
        return Err(INVALID);
    };
    if image.width() > PREVIEW_W || image.height() > PREVIEW_H {
        Ok(image.thumbnail(PREVIEW_W, PREVIEW_H))
    } else {
        Ok(image)
    }
}

#[cfg(test)]
mod tests;
