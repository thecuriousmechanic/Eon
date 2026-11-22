//! Explicitly selected, alternate Apache-2.0 graph; never substitute it at the default model path.

use std::path::{Path, PathBuf};
use std::process::Command;

use eon_voice::tts::KokoroTts;

pub fn load(models: &Path) -> KokoroTts {
    // https://github.com/thewh1teagle/kokoro-onnx/releases/tag/model-files-v1.0
    // kokoro-v1.0.int8.onnx is the taylorchu export (tokens/style/speed -> audio).
    // voices-v1.0.bin is an NPZ; the little-endian f32 payload of af_heart.npy is the locked voice.
    let graph =
        PathBuf::from(std::env::var_os("EON_KOKORO_RELEASE_INT8").expect("set the explicit release graph path"));
    let voice = models.join("tts/voices/af_heart.bin");
    for (path, expected) in [
        (
            &graph,
            "6e742170d309016e5891a994e1ce1559c702a2ccd0075e67ef7157974f6406cb",
        ),
        (
            &voice,
            "d583ccff3cdca2f7fae535cb998ac07e9fcb90f09737b9a41fa2734ec44a8f0b",
        ),
    ] {
        let out = Command::new("sha256sum")
            .arg("--")
            .arg(path)
            .output()
            .expect("requires sha256sum");
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).split_whitespace().next(),
            Some(expected)
        );
    }
    KokoroTts::load(graph, voice).unwrap()
}
