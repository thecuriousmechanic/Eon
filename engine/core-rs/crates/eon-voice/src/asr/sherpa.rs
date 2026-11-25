//! Binding to the online C API shipped by sherpa-onnx v1.12.20.

use std::ffi::{CStr, CString, c_char};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::ptr::NonNull;

use eon_primitives::{EonError, EonResult};
use sha2::{Digest, Sha256};

use super::{DECODER_FILE, ENCODER_FILE, JOINER_FILE, TOKENS_FILE};

// Opaque pointer targets. Rust never constructs or dereferences these.
enum Recognizer {}
enum OnlineStream {}
enum RecognitionResult {}

unsafe extern "C" {
    fn eon_sherpa_create(
        encoder: *const c_char,
        decoder: *const c_char,
        joiner: *const c_char,
        tokens: *const c_char,
    ) -> *const Recognizer;
    fn eon_sherpa_text(result: *const RecognitionResult) -> *const c_char;
    fn SherpaOnnxDestroyOnlineRecognizer(model: *const Recognizer);
    fn SherpaOnnxCreateOnlineStream(model: *const Recognizer) -> *const OnlineStream;
    fn SherpaOnnxDestroyOnlineStream(stream: *const OnlineStream);
    fn SherpaOnnxOnlineStreamAcceptWaveform(stream: *const OnlineStream, rate: i32, pcm: *const f32, n: i32);
    fn SherpaOnnxIsOnlineStreamReady(model: *const Recognizer, stream: *const OnlineStream) -> i32;
    fn SherpaOnnxDecodeOnlineStream(model: *const Recognizer, stream: *const OnlineStream);
    fn SherpaOnnxGetOnlineStreamResult(
        model: *const Recognizer,
        stream: *const OnlineStream,
    ) -> *const RecognitionResult;
    fn SherpaOnnxDestroyOnlineRecognizerResult(result: *const RecognitionResult);
    fn SherpaOnnxOnlineStreamInputFinished(stream: *const OnlineStream);
}

fn error(reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: "zipformer.sherpa".into(),
        reason: reason.to_string(),
    }
}

/// Validate before passing paths to native code. Model installation must not
/// change concurrently. Use the same release pins as the existing downloader.
fn verified_path(dir: &Path, lock: &serde_json::Value, component: &str, filename: &str) -> EonResult<CString> {
    let id = format!("zipformer-en-2023-06-26-{component}");
    let asset = lock["models"]
        .as_array()
        .and_then(|assets| assets.iter().find(|a| a["id"] == id))
        .ok_or_else(|| error(format!("no pin for {id}")))?;
    let size = asset["size_bytes"].as_u64().ok_or_else(|| error("missing size"))?;
    let sha256 = asset["sha256"].as_str().ok_or_else(|| error("missing digest"))?;
    let path = dir.join(filename);
    let metadata = path.metadata().map_err(|e| error(format!("{filename}: {e}")))?;
    if !metadata.is_file() || metadata.len() != size {
        return Err(error(format!("wrong size or type: {filename}")));
    }
    let mut digest = Sha256::new();
    let mut input = File::open(&path).map_err(error)?.take(size + 1);
    if io::copy(&mut input, &mut digest).map_err(error)? != size || format!("{:x}", digest.finalize()) != sha256 {
        return Err(error(format!("digest mismatch: {filename}")));
    }
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(error)
}

pub(super) struct Model(NonNull<Recognizer>);

// The CPU recognizer has no thread affinity. It has one owner; Rust requires
// an exclusive borrow for the entire lifetime of its only active stream.
unsafe impl Send for Model {}

impl Model {
    pub fn load(dir: &Path) -> EonResult<Self> {
        let lock = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../../scripts/models.lock.json"
        )))
        .map_err(error)?;
        let tokens = verified_path(dir, &lock, "tokens", TOKENS_FILE)?;
        let encoder = verified_path(dir, &lock, "encoder-int8", ENCODER_FILE)?;
        let decoder = verified_path(dir, &lock, "decoder", DECODER_FILE)?;
        let joiner = verified_path(dir, &lock, "joiner-int8", JOINER_FILE)?;
        // SAFETY: the C constructor consumes the config synchronously and
        // copies its strings. All four NUL-terminated paths outlive that call.
        let raw = unsafe { eon_sherpa_create(encoder.as_ptr(), decoder.as_ptr(), joiner.as_ptr(), tokens.as_ptr()) };
        NonNull::new(raw.cast_mut())
            .map(Self)
            .ok_or_else(|| error("cannot create v1.12.20 CPU online recognizer"))
    }

    pub fn start_utterance(&mut self) -> EonResult<Stream<'_>> {
        // SAFETY: borrow keeps the model alive until Stream drops.
        let raw = unsafe { SherpaOnnxCreateOnlineStream(self.0.as_ptr()) };
        Ok(Stream {
            raw: NonNull::new(raw.cast_mut()).ok_or_else(|| error("cannot create online stream"))?,
            model: self,
        })
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        // SAFETY: the unique owner, with no remaining streams.
        unsafe { SherpaOnnxDestroyOnlineRecognizer(self.0.as_ptr()) }
    }
}

pub(super) struct Stream<'a> {
    raw: NonNull<OnlineStream>,
    model: &'a mut Model,
}

impl Stream<'_> {
    fn accept(&mut self, pcm: &[f32]) {
        // SAFETY: push validated the input <= 120 * 16_000 samples (fits
        // i32). The pinned library consumes the slice during this call.
        unsafe { SherpaOnnxOnlineStreamAcceptWaveform(self.raw.as_ptr(), 16_000, pcm.as_ptr(), pcm.len() as i32) }
    }

    fn decode_ready(&mut self) -> bool {
        let mut decoded = false;
        // SAFETY: both handles are alive, belong together and are exclusive.
        unsafe {
            while SherpaOnnxIsOnlineStreamReady(self.model.0.as_ptr(), self.raw.as_ptr()) == 1 {
                SherpaOnnxDecodeOnlineStream(self.model.0.as_ptr(), self.raw.as_ptr());
                decoded = true;
            }
        }
        decoded
    }

    fn text(&self) -> EonResult<String> {
        // SAFETY: result and its NUL-terminated text are live until destroy.
        // Copy the text before freeing; no native pointer escapes.
        unsafe {
            let result = SherpaOnnxGetOnlineStreamResult(self.model.0.as_ptr(), self.raw.as_ptr());
            if result.is_null() {
                return Err(error("no online result"));
            }
            let text = eon_sherpa_text(result);
            let owned = if text.is_null() {
                Err(error("no online text"))
            } else {
                // Two minutes at one greedy token per encoder frame, with
                // at most 12 UTF-8 bytes per pinned token, fits well below this.
                let text = CStr::from_ptr(text);
                if text.to_bytes().len() > 64 * 1024 {
                    Err(error("oversized online hypothesis"))
                } else {
                    text.to_str().map(|t| t.trim().to_owned()).map_err(error)
                }
            };
            SherpaOnnxDestroyOnlineRecognizerResult(result);
            owned
        }
    }

    pub fn push(&mut self, samples: &[f32]) -> EonResult<Option<String>> {
        self.accept(samples);
        self.decode_ready().then(|| self.text()).transpose()
    }

    pub fn finish(mut self) -> EonResult<String> {
        // Pad the partial right-context window only at EOF, not between pushes.
        self.accept(&[0.0; 8_000]);
        // SAFETY: consumes self; no waveform can be accepted after this call.
        unsafe { SherpaOnnxOnlineStreamInputFinished(self.raw.as_ptr()) };
        self.decode_ready();
        self.text()
    }
}

impl Drop for Stream<'_> {
    fn drop(&mut self) {
        // SAFETY: unique stream owner; the model is still borrowed.
        unsafe { SherpaOnnxDestroyOnlineStream(self.raw.as_ptr()) }
    }
}

#[cfg(test)]
mod asr_tests {
    use super::*;

    #[test]
    fn same_length_wrong_digest_is_rejected_before_native_load() {
        let filename = "Cargo.toml";
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let size = dir.join(filename).metadata().unwrap().len();
        let lock = serde_json::json!({"models": [{
            "id": "zipformer-en-2023-06-26-tokens", "size_bytes": size, "sha256": "0".repeat(64)
        }]});
        let err = verified_path(dir, &lock, "tokens", filename).unwrap_err();
        assert!(err.to_string().contains("digest mismatch"));
    }
}
