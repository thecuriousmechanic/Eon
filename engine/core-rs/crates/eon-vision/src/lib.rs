//! UI geometry, bounded temporal sampling and opt-in local inference on caller-supplied stills.

pub mod keyframes;
#[cfg(feature = "paddle-ocr")]
pub mod ocr;
#[cfg(feature = "omniparser-icons")]
pub mod omniparser;
#[cfg(all(target_os = "linux", feature = "qwen-vl"))]
pub mod qwen_vl;
#[cfg(target_os = "linux")]
pub mod shared;
#[cfg(any(
    feature = "paddle-ocr",
    feature = "omniparser-icons",
    all(target_os = "linux", feature = "qwen-vl")
))]
mod still;

pub use eon_primitives::*;

/// Half-open bounds in the stored JPEG's raster (no EXIF orientation transform).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelBounds {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

/// Representation of a detected visual UI element with normalized bounding box.
#[derive(Debug, Clone, PartialEq)]
pub struct UiBoundingBox {
    pub element_id: u32,
    pub element_type: String, // "button", "input", "text", "icon"
    pub x_min: f32,
    pub y_min: f32,
    pub x_max: f32,
    pub y_max: f32,
    pub text_content: Option<String>,
    pub is_clickable: bool,
    pub mark_id: Option<u32>, // Set-of-Mark discrete visual tag ([1], [2], [3]...)
}

impl UiBoundingBox {
    pub fn centroid(&self) -> (f32, f32) {
        (
            (self.x_min + self.x_max) / 2.0,
            (self.y_min + self.y_max) / 2.0,
        )
    }
}

/// A temporal video frame in the visual ring buffer.
#[derive(Debug, Clone)]
pub struct VideoKeyframe<Jpeg = Vec<u8>> {
    pub sequence_number: u64,
    pub timestamp_ms: u64,
    pub jpeg_bytes: Jpeg,
    pub width: u32,
    pub height: u32,
    pub is_scene_transition: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ui_bounding_box_centroid() {
        let bbox = UiBoundingBox {
            element_id: 1,
            element_type: "button".to_string(),
            x_min: 0.1,
            y_min: 0.2,
            x_max: 0.3,
            y_max: 0.4,
            text_content: Some("Submit".to_string()),
            is_clickable: true,
            mark_id: Some(1),
        };
        let (cx, cy) = bbox.centroid();
        assert!((cx - 0.2).abs() < 1e-5);
        assert!((cy - 0.3).abs() < 1e-5);
    }
}
