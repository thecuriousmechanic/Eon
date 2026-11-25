#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::fd::AsFd;
use std::path::Path;
use std::time::{Duration, Instant};

use eon_vision::qwen_vl::QwenVlReader;
use eon_vision::shared::LiveSharedVisualMemory;
use llama_cpp_2::llama_backend::LlamaBackend;

const SETTINGS: &[u8] = include_bytes!("fixtures/settings.jpg");
const ACCOUNT: &[u8] = include_bytes!("fixtures/account.jpg");

#[test]
fn reads_distinct_selected_stills_and_abstains_on_blank() {
    let history = LiveSharedVisualMemory::new().unwrap();
    let select = |dims, jpeg: &[u8], level| {
        let seq = history
            .observe(dims, &[level; 1024], jpeg)
            .unwrap()
            .unwrap();
        history.select(seq).unwrap().unwrap()
    };
    let mut settings = select((640, 240), SETTINGS, 0);
    std::thread::sleep(Duration::from_millis(210));
    let account = select((1280, 720), ACCOUNT, 255);
    let mut blank = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut blank)
        .encode(&[255; 224 * 224], 224, 224, image::ExtendedColorType::L8)
        .unwrap();
    std::thread::sleep(Duration::from_millis(210));
    let empty = select((224, 224), &blank, 0);
    history.stop();

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../perception/models/vlm");
    let backend = LlamaBackend::init().unwrap();
    let start = Instant::now();
    let mut vlm = QwenVlReader::from_models(
        &backend,
        File::open(dir.join("Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf")).unwrap(),
        File::open(dir.join("mmproj-Qwen2.5-VL-3B-Instruct-f16.gguf")).unwrap(),
    )
    .unwrap();
    eprintln!("Qwen2.5-VL verified load: {:?}", start.elapsed());
    let mut fd = File::from(settings.jpeg_bytes.as_fd().try_clone_to_owned().unwrap());
    fd.seek(SeekFrom::Start(13)).unwrap();
    assert!(vlm.read_selected(&settings, Duration::ZERO).is_err());
    assert!(
        vlm.read_selected(&settings, Duration::from_micros(1))
            .is_err()
    );
    settings.height -= 1;
    assert!(
        vlm.read_selected(&settings, Duration::from_secs(10))
            .is_err()
    );
    settings.height += 1;
    for (frame, labels) in [
        (&settings, &["Settings", "Search", "Cancel"][..]),
        (&account, &["Profile", "Save"][..]),
        (&empty, &["NONE"][..]),
    ] {
        let start = Instant::now();
        let text = vlm.read_selected(frame, Duration::from_secs(10)).unwrap();
        eprintln!(
            "Qwen2.5-VL {}x{}: {:?}, {text:?}",
            frame.width,
            frame.height,
            start.elapsed()
        );
        assert_eq!(text.split_whitespace().collect::<Vec<_>>(), labels);
    }
    assert_eq!(fd.stream_position().unwrap(), 13);
}
