use super::*;
use image::{ImageFormat, Rgba, RgbaImage};

fn png(color: [u8; 4]) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(12, 4, Rgba(color)))
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

#[test]
fn transparent_pixels_do_not_transmit_hidden_rgb() {
    let bytes = png([0x1b, b']', 0x07, 0]);
    let pixels = decode(&bytes).unwrap().to_rgba8();
    assert!(pixels.pixels().all(|p| p.0 == [0, 0, 0, 255]));
}

#[cfg(unix)]
#[test]
fn one_immutable_selection_no_disclosure_no_reopen() {
    use super::super::{Page, WatchState, render_watch_snapshot};
    use std::{fs, os::unix::fs::symlink};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private-\x1b]2;do-not-display\x07");
    let link = dir.path().join("alias");
    let fifo = dir.path().join("pipe");
    rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, rustix::fs::Mode::RUSR).unwrap();
    let regular_error = Selection::load(&path).unwrap_err();
    assert!(!regular_error.contains("private"));
    assert_eq!(Selection::load(&fifo).unwrap_err(), regular_error);
    assert_eq!(Selection::load(dir.path()).unwrap_err(), regular_error);
    symlink(&path, &link).unwrap();
    fs::File::create(&path)
        .unwrap()
        .set_len(MAX_BYTES as u64 + 1)
        .unwrap();
    assert!(Selection::load(&path).unwrap_err().contains("byte limit"));
    fs::write(&path, png([27, 93, 7, 255])).unwrap();
    assert_eq!(Selection::load(&link).unwrap_err(), regular_error);
    let mut state = WatchState {
        image: Selection::load(&path).unwrap(),
        ..WatchState::default()
    };
    fs::write(&path, png([255, 0, 0, 255])).unwrap();
    state.refresh(dir.path());
    assert!(
        !render_watch_snapshot(&state, 80, 16)
            .unwrap()
            .contains("Selected local still.")
    );
    state.open(Page::Vision);
    let Selection::Frame(ref frame) = state.image else {
        panic!("no frame")
    };
    assert_eq!(frame.to_rgb8().get_pixel(0, 0).0, [27, 93, 7]);
    let screen = render_watch_snapshot(&state, 80, 16).unwrap();
    assert!(screen.contains("Selected local still."));
    assert!(!screen.contains("private") && !screen.contains('\x1b'));
    state.open(Page::Vision);
    assert!(matches!(state.image, Selection::Frame(_)));
    state.open(Page::Audio);
    assert!(matches!(state.image, Selection::Released));
    state.open(Page::Vision);
    assert!(
        render_watch_snapshot(&state, 80, 16)
            .unwrap()
            .contains("still released")
    );
    state.image = Selection::load(&path).unwrap();
    state.on_key('q');
    assert!(state.quit && matches!(state.image, Selection::Released));
}

#[test]
fn bounded_before_raster_allocation_including_progressive_jpeg() {
    assert!(
        decode(&vec![0; MAX_BYTES + 1])
            .unwrap_err()
            .contains("byte limit")
    );
    assert!(check_size(1920, 1080).is_ok());
    assert!(check_size(MAX_SIDE + 1, 1).is_err());
    assert!(check_size(2048, 1025).is_err());
    assert!(check_size(u32::MAX, u32::MAX).is_err());
    let mut large = Cursor::new(Vec::new());
    DynamicImage::new_luma8(2048, 1025)
        .write_to(&mut large, ImageFormat::Png)
        .unwrap();
    assert_eq!(
        decode(large.get_ref()).unwrap_err(),
        "selected image exceeds the raster limit"
    );
    let mut small = Cursor::new(Vec::new());
    DynamicImage::new_rgb8(32, 32)
        .write_to(&mut small, ImageFormat::Jpeg)
        .unwrap();
    let mut jpeg = small.into_inner();
    assert!(decode(&jpeg).is_ok());
    let sof = jpeg.windows(2).position(|s| s == [255, 192]).unwrap();
    jpeg[sof + 5..sof + 9].copy_from_slice(&[0x08, 0x00, 0x08, 0x00]);
    for marker in [0xc0, 0xc2] {
        jpeg[sof + 1] = marker;
        // The header, not the entropy stream, must be the reason this fails.
        assert_eq!(
            decode(&jpeg).unwrap_err(),
            "selected image exceeds the raster limit"
        );
    }
}
