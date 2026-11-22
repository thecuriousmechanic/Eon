use std::fs;
use std::path::Path;
use std::time::Duration;

use eon_vision::PixelBounds;
use eon_vision::omniparser::{IconDetector, IconPage};
use image::{Rgb, RgbImage, codecs::jpeg::JpegEncoder};

const BUDGET: Duration = Duration::from_secs(10);
const DIMS: (u32, u32) = (801, 317);
const TILES: [[u32; 4]; 2] = [[111, 61, 165, 116], [514, 149, 568, 203]];

fn model() -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../perception/models/vision/omniparser/icon_detect/model.onnx"),
    )
    .expect("provision omniparser-v2-icon-detect-onnx with scripts/download_models.py")
}

// Two independently drawn, unlabeled toolbar tiles: a magnifying glass and a
// menu. Ground truth comes from the colored tiles, not a model's predictions.
fn toolbar(with_icons: bool) -> RgbImage {
    RgbImage::from_fn(DIMS.0, DIMS.1, |x, y| {
        let (x, y) = (x as i32, y as i32);
        let radius_sq = (x - 134).pow(2) + (y - 85).pow(2);
        let search = (9 * 9..=13 * 13).contains(&radius_sq)
            || ((142..=154).contains(&x) && (x - y - 49).abs() <= 2);
        let menu = (525..557).contains(&x) && (164..188).contains(&y) && (y - 164) % 10 < 4;
        let tile = ((111..165).contains(&x) && (61..116).contains(&y))
            || ((514..568).contains(&x) && (149..203).contains(&y));
        if with_icons && (search || menu) {
            Rgb([255, 255, 255])
        } else if with_icons && tile {
            Rgb([35, 83, 148])
        } else {
            Rgb([245, 246, 248])
        }
    })
}

fn jpeg(image: &RgbImage) -> Vec<u8> {
    let mut bytes = vec![];
    JpegEncoder::new_with_quality(&mut bytes, 92)
        .encode_image(image)
        .unwrap();
    bytes
}

fn overlap(a: PixelBounds, [l, t, r, b]: [u32; 4]) -> f64 {
    let common =
        a.right.min(r).saturating_sub(a.left.max(l)) * a.bottom.min(b).saturating_sub(a.top.max(t));
    f64::from(common)
        / f64::from((a.right - a.left) * (a.bottom - a.top) + (r - l) * (b - t) - common)
}

fn check(page: &IconPage, dims: (u32, u32), tiles: [[u32; 4]; 2]) {
    assert_eq!((page.width, page.height), dims);
    for icon in &page.icons {
        let b = icon.bounds;
        assert!(b.left < b.right && b.right <= page.width);
        assert!(b.top < b.bottom && b.bottom <= page.height);
        assert!((0.25..=1.0).contains(&icon.confidence));
    }
    for ink in tiles {
        assert_eq!(
            page.icons
                .iter()
                .filter(|i| overlap(i.bounds, ink) >= 0.6)
                .count(),
            1,
            "ink {ink:?}: {page:?}"
        );
    }
    assert_eq!(page.icons.len(), 2, "{page:?}");
}

#[test]
fn detects_drawn_icons_not_blank_pixels_and_recovers_after_timeout() {
    let mut detector = IconDetector::from_model(&model()).unwrap();
    let landscape = toolbar(true);
    let icons = jpeg(&landscape);
    assert!(
        detector
            .detect_jpeg(&icons, Duration::from_micros(1))
            .is_err()
    );
    let page = detector.detect_jpeg(&icons, BUDGET).unwrap();
    check(&page, DIMS, TILES);
    eprintln!("landscape toolbar: {:?}", page.icons);
    let rotated = image::imageops::rotate90(&landscape);
    let tiles = TILES.map(|[l, t, r, b]| [DIMS.1 - b, l, DIMS.1 - t, r]);
    check(
        &detector.detect_jpeg(&jpeg(&rotated), BUDGET).unwrap(),
        rotated.dimensions(),
        tiles,
    );
    assert!(
        detector
            .detect_jpeg(&jpeg(&toolbar(false)), BUDGET)
            .unwrap()
            .icons
            .is_empty()
    );
}

#[test]
fn refuses_substituted_graph_before_native_load() {
    let mut graph = model();
    let last = graph.len() - 1;
    graph[last] ^= 1;
    assert!(matches!(
        IconDetector::from_model(&graph),
        Err(eon_vision::EonError::ValidationError(_))
    ));
}

#[test]
fn responds_to_translated_icons_on_a_different_canvas() {
    let mut detector = IconDetector::from_model(&model()).unwrap();
    let image = toolbar(true);
    for (dx, dy) in [(21, 78), (94, 13)] {
        let mut shifted = RgbImage::from_pixel(911, 417, Rgb([245, 246, 248]));
        image::imageops::replace(&mut shifted, &image, dx.into(), dy.into());
        check(
            &detector.detect_jpeg(&jpeg(&shifted), BUDGET).unwrap(),
            shifted.dimensions(),
            TILES.map(|[l, t, r, b]| [l + dx, t + dy, r + dx, b + dy]),
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn detects_only_the_selected_sealed_still() {
    use eon_vision::shared::LiveSharedVisualMemory;
    use std::{
        fs::File,
        io::{Seek, SeekFrom},
        os::fd::AsFd,
    };

    let mut detector = IconDetector::from_model(&model()).unwrap();
    let history = LiveSharedVisualMemory::new().unwrap();
    let seq = history
        .observe(DIMS, &[0; 1024], &jpeg(&toolbar(true)))
        .unwrap()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let newer = history
        .observe(DIMS, &[255; 1024], &jpeg(&toolbar(false)))
        .unwrap()
        .unwrap();
    assert_ne!(seq, newer);
    let mut frame = history.select(seq).unwrap().unwrap();
    history.stop();
    let mut fd = File::from(frame.jpeg_bytes.as_fd().try_clone_to_owned().unwrap());
    fd.seek(SeekFrom::Start(13)).unwrap();
    check(
        &detector.detect_selected(&frame, BUDGET).unwrap(),
        DIMS,
        TILES,
    );
    assert_eq!(fd.stream_position().unwrap(), 13);
    frame.height -= 1;
    assert!(matches!(
        detector.detect_selected(&frame, BUDGET),
        Err(eon_vision::EonError::ValidationError(_))
    ));
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "hardware-dependent release-mode p95 < 30 ms acceptance gate"]
fn selected_still_cpu_latency() {
    use eon_vision::shared::LiveSharedVisualMemory;
    use std::time::Instant;

    let history = LiveSharedVisualMemory::new().unwrap();
    let seq = history
        .observe(DIMS, &[0; 1024], &jpeg(&toolbar(true)))
        .unwrap()
        .unwrap();
    let selected = history.select(seq).unwrap().unwrap();
    history.stop();
    let bytes = model();
    let start = Instant::now();
    let mut detector = IconDetector::from_model(&bytes).unwrap();
    let load = start.elapsed();
    let mut sample = || {
        let start = Instant::now();
        let page = detector.detect_selected(&selected, BUDGET).unwrap();
        let elapsed = start.elapsed();
        check(&page, DIMS, TILES);
        elapsed
    };
    let first = sample();
    for _ in 0..4 {
        sample();
    }
    let mut warm: Vec<_> = (0..30).map(|_| sample()).collect();
    warm.sort_unstable();
    let p95 = warm[28];
    eprintln!(
        "801x317 selected JPEG, {}: load={load:?}, first={first:?}; n=30 warm p50={:?} p95={p95:?} max={:?}",
        detector.backend_name(),
        warm[14],
        warm[29]
    );
    assert!(
        p95 < Duration::from_millis(30),
        "30 ms icon target missed: p95={p95:?}"
    );
}
