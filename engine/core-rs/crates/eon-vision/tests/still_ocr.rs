//! Exercises the released PP-OCRv4 graphs. See the `eon-vision` OCR subsection of
//! `docs/TECH_STACK_AND_TOOLCHAINS.md` for offline model provisioning.
use std::{fs, path::PathBuf, time::Duration};

use eon_vision::ocr::{OcrPage, PaddleOcr, PixelBounds};

const SETTINGS: &[u8] = include_bytes!("fixtures/settings.jpg");
const ACCOUNT: &[u8] = include_bytes!("fixtures/account.jpg");
const BUDGET: Duration = Duration::from_secs(10);

fn models() -> (Vec<u8>, Vec<u8>) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../perception/models/vision/ppocr-v4");
    let read = |name| fs::read(dir.join(name)).expect("provision the pinned PP-OCRv4 graphs first");
    (
        read("ch_PP-OCRv4_det_infer.onnx"),
        read("ch_PP-OCRv4_rec_infer.onnx"),
    )
}

fn check(page: &OcrPage, dimensions: (u32, u32), expected: &[(&str, [u32; 4])]) {
    assert_eq!((page.width, page.height), dimensions);
    assert_eq!(page.lines.len(), expected.len(), "{page:?}");
    for (line, &(label, edges)) in page.lines.iter().zip(expected) {
        assert_eq!(line.text, label, "{page:?}");
        assert!((0.6..=1.0).contains(&line.confidence));
        let PixelBounds {
            left,
            top,
            right,
            bottom,
        } = line.bounds;
        // Compare all four OCR edges with the independently rasterized text's ink
        // extent, allowing the text detector a modest margin around the glyphs.
        for (detected, ink) in [left, top, right, bottom].into_iter().zip(edges) {
            assert!(
                detected.abs_diff(ink) <= 14,
                "{label}: {:?} vs {edges:?}",
                line.bounds
            );
        }
    }
}

#[test]
fn grounds_independently_rendered_labels_in_reading_order() {
    let (det, rec) = models();
    // Another model worker may already have initialized ORT.
    eon_primitives::onnx::environment().unwrap();
    let mut ocr = PaddleOcr::from_models(&det, &rec).unwrap();
    check(
        &ocr.recognize_jpeg(SETTINGS, BUDGET).unwrap(),
        (640, 240),
        &[
            ("Settings", [40, 39, 168, 70]),
            ("Search", [395, 36, 500, 60]),
            ("Cancel", [89, 166, 193, 190]),
        ],
    );
    drop(ocr);
    let mut ocr = PaddleOcr::from_models(&det, &rec).unwrap();
    check(
        &ocr.recognize_jpeg(ACCOUNT, BUDGET).unwrap(),
        (1280, 720),
        &[
            ("Profile", [864, 169, 980, 200]),
            ("Save", [148, 525, 241, 555]),
        ],
    );
}

#[test]
fn corrupt_recognizer_with_correct_length_is_refused() {
    let (det, mut rec) = models();
    rec[12] ^= 1;
    assert!(matches!(
        PaddleOcr::from_models(&det, &rec),
        Err(eon_vision::EonError::ValidationError(_))
    ));
}

#[test]
fn deadline_returns_no_partial_page_and_blank_pixels_produce_no_text() {
    let (det, rec) = models();
    let mut ocr = PaddleOcr::from_models(&det, &rec).unwrap();
    assert!(
        ocr.recognize_jpeg(ACCOUNT, Duration::from_micros(1))
            .is_err()
    );
    let mut blank = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut blank)
        .encode(&[255; 128 * 64], 128, 64, image::ExtendedColorType::L8)
        .unwrap();
    assert!(ocr.recognize_jpeg(&blank, BUDGET).unwrap().lines.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn sealed_selection_not_the_latest_frame_is_grounded_after_stop() {
    use eon_vision::shared::LiveSharedVisualMemory;
    use std::{
        fs::File,
        io::{Seek, SeekFrom},
        os::fd::AsFd,
    };

    let (det, rec) = models();
    let mut ocr = PaddleOcr::from_models(&det, &rec).unwrap();
    let history = LiveSharedVisualMemory::new().unwrap();
    let seq = history
        .observe((1280, 720), &[0; 1024], ACCOUNT)
        .unwrap()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let newer = history
        .observe((640, 240), &[255; 1024], SETTINGS)
        .unwrap()
        .unwrap();
    assert_ne!(seq, newer);
    let mut selected = history.select(seq).unwrap().unwrap();
    history.stop();
    let mut fd = File::from(selected.jpeg_bytes.as_fd().try_clone_to_owned().unwrap());
    fd.seek(SeekFrom::Start(37)).unwrap();
    let page = ocr.recognize_selected(&selected, BUDGET).unwrap();
    assert_eq!(
        page.lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>(),
        ["Profile", "Save"]
    );
    assert_eq!(fd.stream_position().unwrap(), 37);
    selected.width -= 1;
    assert!(matches!(
        ocr.recognize_selected(&selected, BUDGET),
        Err(eon_vision::EonError::ValidationError(_))
    ));
}
