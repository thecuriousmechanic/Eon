//! Local online Zipformer ASR. A recognizer owns the loaded model; each utterance
//! gets fresh feature/decoder state. Feed normalized 16 kHz mono PCM on a worker,
//! not on the real-time audio callback.

use std::path::Path;

use eon_primitives::{EonError, EonResult};

use crate::{endpoint::ReflexHint, pre_cognition::IntentPreview};

#[cfg(feature = "onnx-speech")]
mod direct;
#[cfg(feature = "sherpa-onnx")]
mod sherpa;

const ENCODER_FILE: &str = "encoder-epoch-99-avg-1-chunk-16-left-128.int8.onnx";
const DECODER_FILE: &str = "decoder-epoch-99-avg-1-chunk-16-left-128.onnx";
const JOINER_FILE: &str = "joiner-epoch-99-avg-1-chunk-16-left-128.int8.onnx";
const TOKENS_FILE: &str = "tokens.txt";
pub const MAX_UTTERANCE_SAMPLES: usize = 120 * 16_000;

enum Backend {
    #[cfg(feature = "onnx-speech")]
    Ort(Box<direct::Model>),
    #[cfg(feature = "sherpa-onnx")]
    Sherpa(sherpa::Model),
}

pub struct ZipformerAsr(Backend);

impl ZipformerAsr {
    /// With `sherpa-onnx`, select the Sherpa C++ online recognizer; otherwise
    /// use the original direct-ORT implementation. Never download or fall back.
    pub fn load(model_dir: impl AsRef<Path>) -> EonResult<Self> {
        #[cfg(feature = "sherpa-onnx")]
        {
            sherpa::Model::load(model_dir.as_ref()).map(|m| Self(Backend::Sherpa(m)))
        }
        #[cfg(not(feature = "sherpa-onnx"))]
        Self::load_ort(model_dir)
    }

    /// Explicit direct-ORT backend, including when both features are enabled.
    #[cfg(feature = "onnx-speech")]
    pub fn load_ort(model_dir: impl AsRef<Path>) -> EonResult<Self> {
        direct::Model::load(model_dir).map(|m| Self(Backend::Ort(Box::new(m))))
    }

    pub fn start_utterance(&mut self) -> EonResult<ZipformerStream<'_>> {
        let decoder = match &mut self.0 {
            #[cfg(feature = "onnx-speech")]
            Backend::Ort(model) => Decoder::Ort(Box::new(model.start_utterance()?)),
            #[cfg(feature = "sherpa-onnx")]
            Backend::Sherpa(model) => Decoder::Sherpa(model.start_utterance()?),
        };
        Ok(ZipformerStream {
            decoder,
            input_samples: 0,
            last_partial: String::new(),
        })
    }

    pub fn transcribe(&mut self, samples: &[f32]) -> EonResult<String> {
        let mut stream = self.start_utterance()?;
        stream.push(samples)?;
        stream.finish()
    }
}

enum Decoder<'a> {
    #[cfg(feature = "onnx-speech")]
    Ort(Box<direct::Stream<'a>>),
    #[cfg(feature = "sherpa-onnx")]
    Sherpa(sherpa::Stream<'a>),
}

pub struct ZipformerStream<'a> {
    decoder: Decoder<'a>,
    input_samples: usize,
    last_partial: String,
}

impl ZipformerStream<'_> {
    /// Returns only changed text. A new snapshot replaces the entire previous
    /// preview (including its intent); `None` means no change.
    pub fn push(&mut self, samples: &[f32]) -> EonResult<Option<PartialTranscript>> {
        if samples.len() > MAX_UTTERANCE_SAMPLES - self.input_samples {
            return Err(EonError::ValidationError(
                "utterance exceeds the two-minute limit".into(),
            ));
        }
        if !samples.iter().all(|s| (-1.0..=1.0).contains(s)) {
            return Err(EonError::ValidationError("expected finite, normalized mono PCM".into()));
        }
        if samples.is_empty() {
            return Ok(None);
        }
        self.input_samples += samples.len();
        let text = match &mut self.decoder {
            #[cfg(feature = "onnx-speech")]
            Decoder::Ort(stream) => stream.push(samples)?,
            #[cfg(feature = "sherpa-onnx")]
            Decoder::Sherpa(stream) => stream.push(samples)?,
        };
        Ok(text.filter(|t| t != &self.last_partial).map(|text| {
            self.last_partial.clone_from(&text);
            let intent = IntentPreview::from_partial(&text);
            PartialTranscript {
                hint: match intent {
                    Some(IntentPreview::Reflex(hint)) => Some(hint),
                    _ => None,
                },
                text,
                intent,
            }
        }))
    }

    /// Flushes the right acoustic context exactly once. Dropping instead
    /// cancels this utterance without emitting a final or retaining a preview.
    pub fn finish(self) -> EonResult<String> {
        match self.decoder {
            #[cfg(feature = "onnx-speech")]
            Decoder::Ort(stream) => stream.finish().map(|(text, _)| text),
            #[cfg(feature = "sherpa-onnx")]
            Decoder::Sherpa(stream) => stream.finish(),
        }
    }

    /// Append-only, nonblank **text** token IDs actually selected by the greedy
    /// transducer. Never reconstruct IDs by splitting a transcript.
    #[cfg(feature = "onnx-speech")]
    pub fn text_token_ids(&self) -> Option<&[i64]> {
        match &self.decoder {
            Decoder::Ort(stream) => Some(stream.text_token_ids()),
            #[cfg(feature = "sherpa-onnx")]
            Decoder::Sherpa(_) => None,
        }
    }

    /// Include IDs emitted while flushing the final acoustic context.
    #[cfg(feature = "onnx-speech")]
    pub fn finish_with_text_tokens(self) -> EonResult<(String, Vec<i64>)> {
        match self.decoder {
            Decoder::Ort(stream) => stream.finish(),
            #[cfg(feature = "sherpa-onnx")]
            Decoder::Sherpa(_) => Err(EonError::CapabilityNotFound {
                capability_id: "direct Zipformer text-token stream".into(),
            }),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct PartialTranscript {
    pub text: String,
    /// Compatibility view of a closed reflex; never authorizes an action.
    pub hint: Option<ReflexHint>,
    pub intent: Option<IntentPreview>,
}
