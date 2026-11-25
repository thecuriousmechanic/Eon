//! Kokoro-82M text-to-speech (docs/implementation/04, TTS section).
//!
//! The pipeline is text to phonemes (espeak-ng, en-us, IPA with stress marks), phonemes to token ids through the
//! model's vocabulary, then one ONNX call that returns a 24 kHz waveform. The voice file holds one style vector for each
//! token count, so the style row is picked by the number of tokens, as in the reference implementation.
//!
//! espeak-ng reads the text from stdin and is never given it as an argument, so text cannot inject options.

mod phonemizer;

use std::collections::HashMap;
use std::path::Path;

use eon_primitives::{EonError, EonResult};
use ort::session::Session;
use ort::value::Tensor;

pub const SAMPLE_RATE_HZ: u32 = 24_000;
/// Refuse excess text rather than silently speaking a truncated claim.
const MAX_TOKENS: usize = 509;
pub(crate) const MAX_CLAUSE_BYTES: usize = 2048;
const MAX_CLAUSE_SAMPLES: usize = SAMPLE_RATE_HZ as usize * 60;
const STYLE_DIM: usize = 256;
const VOICE_ROWS: usize = 510;
const SPEED_RANGE: std::ops::RangeInclusive<f32> = 0.5..=2.0;
const VOCAB_JSON: &str = include_str!("../assets/kokoro_vocab.json");

fn tts_err(operation: &str, reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: format!("tts.{operation}"),
        reason: reason.to_string(),
    }
}

fn load_vocab() -> EonResult<HashMap<char, i64>> {
    let root: serde_json::Value = serde_json::from_str(VOCAB_JSON).map_err(|e| tts_err("vocab", e))?;
    let entries = root
        .get("vocab")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| tts_err("vocab", "missing vocab object"))?;
    entries
        .iter()
        .map(|(symbol, id)| {
            let mut chars = symbol.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return Err(tts_err("vocab", format!("entry {symbol:?} is not one character")));
            };
            let id = id
                .as_i64()
                .ok_or_else(|| tts_err("vocab", format!("id for {symbol:?} is not an integer")))?;
            Ok((c, id))
        })
        .collect()
}

/// Converts text to en-us IPA phonemes with stress marks. Newlines become spaces and runs of spaces collapse.
pub fn phonemize(text: &str) -> EonResult<String> {
    phonemizer::run(text, || false)
}

pub struct KokoroTts {
    session: Session,
    ports: (&'static str, &'static str),
    frontend: KokoroFrontend,
    voice: Vec<f32>,
}

/// Pronunciation only. Does not load the waveform model or open an audio device.
pub struct KokoroFrontend {
    vocab: HashMap<char, i64>,
}

/// One local pronunciation, or a peer's byte-identical verified pronunciation.
/// There is deliberately no constructor accepting unchecked network token IDs.
pub struct PreparedClause {
    text: String,
    ids: Vec<u8>,
}

impl PreparedClause {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn ids(&self) -> &[u8] {
        &self.ids
    }

    /// Reject mismatches without losing the checked local fallback.
    pub fn accept_verified_ids(&mut self, received: Vec<u8>) -> bool {
        if self.ids != received {
            return false;
        }
        self.ids = received;
        true
    }
}

impl KokoroFrontend {
    pub fn load() -> EonResult<Self> {
        Ok(Self { vocab: load_vocab()? })
    }

    pub fn prepare(&self, text: &str) -> EonResult<PreparedClause> {
        self.prepare_cancellable(text, || false)
    }

    /// Like `prepare`, but stops the phonemizer when the caller revokes this clause.
    /// `cancelled` must be a cheap, nonblocking check.
    pub fn prepare_cancellable(&self, text: &str, cancelled: impl Fn() -> bool) -> EonResult<PreparedClause> {
        let ids = phonemizer::run(text, cancelled)?
            .chars()
            .filter_map(|c| self.vocab.get(&c))
            .take(MAX_TOKENS + 1)
            .map(|&id| u8::try_from(id).map_err(|_| tts_err("vocab", "invalid phoneme ID")))
            .collect::<EonResult<Vec<_>>>()?;
        if ids.is_empty() || ids.len() > MAX_TOKENS {
            return Err(EonError::ValidationError(
                "expected 1..=509 Kokoro phoneme tokens".into(),
            ));
        }
        Ok(PreparedClause {
            text: text.to_owned(),
            ids,
        })
    }
}

impl KokoroTts {
    /// Loads a caller-verified Kokoro v1 graph and raw style bank. The default artifacts are pinned in
    /// `scripts/models.lock.json`; the separate `thewh1teagle/kokoro-onnx` v1 release uses `tokens` / `audio`.
    /// There is no implicit model lookup, download or fallback.
    pub fn load(model: impl AsRef<Path>, voice: impl AsRef<Path>) -> EonResult<Self> {
        // A clause is a single blocking graph run. Respect the host CPU limit and cap
        // inference parallelism so the audio callback and perception worker can still run.
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
        let session = Session::builder(crate::runtime::environment()?)
            .map_err(|e| tts_err("builder", e))?
            .with_intra_threads(threads)
            .map_err(|e| tts_err("threads", e))?
            .commit_from_file(model)
            .map_err(|e| tts_err("load", e))?;
        let ports = [("input_ids", "waveform"), ("tokens", "audio")]
            .into_iter()
            .find(|(ids, pcm)| {
                session.inputs().len() == 3
                    && [*ids, "style", "speed"]
                        .iter()
                        .all(|n| session.inputs().iter().any(|i| i.name() == *n))
                    && session.outputs().iter().any(|o| o.name() == *pcm)
            })
            .ok_or_else(|| tts_err("load", "unsupported Kokoro input/output names"))?;
        let bytes = std::fs::read(voice).map_err(|e| tts_err("voice", e))?;
        let expected = VOICE_ROWS * STYLE_DIM * 4;
        if bytes.len() != expected {
            return Err(tts_err(
                "voice",
                format!("expected {expected} bytes, found {}", bytes.len()),
            ));
        }
        let voice = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        Ok(Self {
            session,
            ports,
            frontend: KokoroFrontend::load()?,
            voice,
        })
    }

    /// Synthesizes `text` at `speed` (0.5 to 2.0) and returns mono samples at `SAMPLE_RATE_HZ`.
    /// Synthesizes clause by clause and hands each clause's samples to `on_clause` as soon as they exist. This lets
    /// playback start before the whole reply is synthesized.
    pub fn synthesize_streaming(
        &mut self,
        text: &str,
        speed: f32,
        mut on_clause: impl FnMut(&[f32]),
    ) -> EonResult<StreamTiming> {
        let started = std::time::Instant::now();
        let mut count = 0;
        let mut first_audio = None;
        for clause in clauses(text) {
            let samples = self.synthesize(clause, speed)?;
            first_audio.get_or_insert_with(|| started.elapsed());
            on_clause(&samples);
            count += 1;
        }
        Ok(StreamTiming {
            clauses: count,
            first_audio: first_audio
                .ok_or_else(|| EonError::ValidationError("text has no speakable clauses".into()))?,
            total: started.elapsed(),
        })
    }

    pub fn synthesize(&mut self, text: &str, speed: f32) -> EonResult<Vec<f32>> {
        self.synthesize_cancellable(text, speed, || false)
    }

    pub(crate) fn synthesize_cancellable(
        &mut self,
        text: &str,
        speed: f32,
        cancelled: impl Fn() -> bool,
    ) -> EonResult<Vec<f32>> {
        if !SPEED_RANGE.contains(&speed) {
            return Err(EonError::ValidationError(format!(
                "speed {speed} is outside 0.5 to 2.0"
            )));
        }
        let clause = self.frontend.prepare_cancellable(text, cancelled)?;
        self.synthesize_prepared(&clause, speed)
    }

    pub(crate) fn synthesize_prepared(&mut self, clause: &PreparedClause, speed: f32) -> EonResult<Vec<f32>> {
        if !SPEED_RANGE.contains(&speed) {
            return Err(EonError::ValidationError("invalid Kokoro playback speed".into()));
        }
        let row = clause.ids.len();
        let style = self.voice[row * STYLE_DIM..(row + 1) * STYLE_DIM].to_vec();
        let mut ids = Vec::with_capacity(row + 2);
        ids.push(0);
        ids.extend(clause.ids.iter().map(|&id| i64::from(id)));
        ids.push(0);

        let input_ids = Tensor::from_array(([1usize, ids.len()], ids)).map_err(|e| tts_err("input", e))?;
        let style = Tensor::from_array(([1usize, STYLE_DIM], style)).map_err(|e| tts_err("input", e))?;
        let speed = Tensor::from_array(([1usize], vec![speed])).map_err(|e| tts_err("input", e))?;
        let outputs = self
            .session
            .run(ort::inputs![self.ports.0 => input_ids, "style" => style, "speed" => speed])
            .map_err(|e| tts_err("run", e))?;
        let (_, waveform) = outputs[self.ports.1]
            .try_extract_tensor::<f32>()
            .map_err(|e| tts_err("waveform", e))?;
        if waveform.is_empty() || waveform.len() > MAX_CLAUSE_SAMPLES || !waveform.iter().all(|s| s.is_finite()) {
            return Err(tts_err("waveform", "expected 0 < duration <= 60s of finite mono PCM"));
        }
        Ok(waveform.to_vec())
    }
}

/// Splits text into clauses after sentence and clause punctuation. Each clause keeps its punctuation. Empty pieces are
/// dropped, so every returned clause has at least one speakable character.
pub fn split_clauses(text: &str) -> Vec<String> {
    clauses(text).map(str::to_owned).collect()
}

pub(crate) fn clauses(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive(['.', '!', '?', ';', ':', ','])
        .map(str::trim)
        .filter(|s| s.chars().any(char::is_alphanumeric))
}

/// Timing of one streamed synthesis. `first_audio` is the time until the first clause's samples were delivered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamTiming {
    pub clauses: usize,
    pub first_audio: std::time::Duration,
    pub total: std::time::Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clauses_keep_their_punctuation_and_drop_empty_pieces() {
        assert_eq!(
            split_clauses("Affirmative. I am running the test now, and the report follows shortly."),
            vec![
                "Affirmative.",
                "I am running the test now,",
                "and the report follows shortly."
            ]
        );
        assert_eq!(split_clauses("  ... ,  "), Vec::<String>::new());
        assert_eq!(split_clauses("no punctuation here"), vec!["no punctuation here"]);
    }

    #[test]
    fn vocab_maps_single_characters_to_the_reference_ids() {
        let vocab = load_vocab().unwrap();
        assert_eq!(vocab[&'a'], 43);
        assert_eq!(vocab[&' '], 16);
        assert_eq!(vocab[&'ˈ'], 156);
        assert_eq!(vocab.len(), 114);
    }
}
