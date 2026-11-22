//! Zipformer streaming transducer ASR over ONNX Runtime (docs/implementation/04, speech recognition section).
//!
//! The encoder consumes 45-frame chunks of 80-bin Kaldi fbank and carries its cache tensors from one chunk to the
//! next. Greedy transducer search runs the joiner on every encoder frame and advances a stateful decoder over the
//! last two emitted tokens. Token 0 is blank.

use std::collections::HashMap;
use std::path::Path;

use eon_primitives::{EonError, EonResult};

use super::{DECODER_FILE, ENCODER_FILE, JOINER_FILE, TOKENS_FILE};
use crate::features::{NUM_MEL_BINS, SAMPLE_RATE_HZ, online_fbank};
use kaldi_native_fbank::OnlineFeature;
use ort::session::{Session, SessionInputValue};
use ort::value::{DynValue, Tensor, TensorElementType, TensorRef, ValueType};

const CHUNK_FRAMES: usize = 45;
const CHUNK_SHIFT: usize = 32;
const CONTEXT_SIZE: usize = 2;
const BLANK_ID: i64 = 0;
const WORD_BOUNDARY: char = '\u{2581}';

struct CacheInput {
    name: String,
    output_name: String,
    zero: DynValue,
}

pub(super) struct Model {
    encoder: Session,
    decoder: Session,
    joiner: Session,
    tokens: HashMap<i64, String>,
    cache_inputs: Vec<CacheInput>,
}

fn onnx_err(context: &str, e: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: format!("zipformer.{context}"),
        reason: e.to_string(),
    }
}

fn load_session(path: &Path) -> EonResult<Session> {
    Session::builder(crate::runtime::environment()?)
        .map_err(|e| onnx_err("builder", e))?
        .with_intra_threads(1)
        .map_err(|e| onnx_err("threads", e))?
        .commit_from_file(path)
        .map_err(|e| onnx_err("load", e))
}

fn load_tokens(path: &Path) -> EonResult<HashMap<i64, String>> {
    let text = std::fs::read_to_string(path).map_err(|e| onnx_err("tokens", e))?;
    text.lines()
        .filter_map(|line| line.rsplit_once(' '))
        .map(|(symbol, id)| {
            let id = id.parse::<i64>().map_err(|e| onnx_err("tokens", e))?;
            Ok((id, symbol.to_owned()))
        })
        .collect()
}

/// Zero-initialises every encoder input except the audio features. Dynamic dimensions are sized to 1, which matches
/// the batch size used at inference.
fn initial_caches(encoder: &Session) -> EonResult<Vec<CacheInput>> {
    encoder
        .inputs()
        .iter()
        .filter(|input| input.name() != "x")
        .map(|input| {
            let ValueType::Tensor { ty, shape, .. } = input.dtype() else {
                return Err(onnx_err("caches", format!("non-tensor input {}", input.name())));
            };
            let dims: Vec<i64> = shape.iter().map(|&d| if d <= 0 { 1 } else { d }).collect();
            let len = dims.iter().product::<i64>() as usize;
            let zero = match ty {
                TensorElementType::Float32 => Tensor::from_array((dims, vec![0.0_f32; len])).map(Tensor::into_dyn),
                TensorElementType::Int64 => Tensor::from_array((dims, vec![0_i64; len])).map(Tensor::into_dyn),
                other => {
                    return Err(onnx_err(
                        "caches",
                        format!("unsupported {other:?} for {}", input.name()),
                    ));
                }
            }
            .map_err(|e| onnx_err("cache", e))?;
            Ok(CacheInput {
                name: input.name().to_owned(),
                output_name: format!("new_{}", input.name()),
                zero,
            })
        })
        .collect()
}

fn argmax(values: &[f32]) -> i64 {
    values
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i as i64)
        .unwrap_or(BLANK_ID)
}

impl Model {
    /// Loads the three ONNX graphs and token table from `model_dir`. Files must be the digest-verified artifacts from
    /// `scripts/models.lock.json` (`zipformer-*`).
    pub fn load(model_dir: impl AsRef<Path>) -> EonResult<Self> {
        let dir = model_dir.as_ref();
        let encoder = load_session(&dir.join(ENCODER_FILE))?;
        let cache_inputs = initial_caches(&encoder)?;
        Ok(Self {
            decoder: load_session(&dir.join(DECODER_FILE))?,
            joiner: load_session(&dir.join(JOINER_FILE))?,
            tokens: load_tokens(&dir.join(TOKENS_FILE))?,
            encoder,
            cache_inputs,
        })
    }

    pub fn start_utterance(&mut self) -> EonResult<Stream<'_>> {
        let context = [BLANK_ID; CONTEXT_SIZE];
        let decoder_out = self.run_decoder(context)?;
        Ok(Stream {
            model: self,
            caches: None,
            features: online_fbank()?,
            next_frame: 0,
            chunk: Tensor::from_array((
                [1usize, CHUNK_FRAMES, NUM_MEL_BINS],
                vec![0.0f32; CHUNK_FRAMES * NUM_MEL_BINS],
            ))
            .map_err(|e| onnx_err("input", e))?,
            context,
            decoder_out,
            tokens: Vec::new(),
        })
    }
}

/// One utterance. Both the overlapping fbank windows and the transducer state
/// survive arbitrary PCM push boundaries. End padding is added only by `finish`.
pub(super) struct Stream<'a> {
    model: &'a mut Model,
    // None feeds the immutable zeros. Only the stream owns its subsequent encoder states.
    caches: Option<Vec<DynValue>>,
    features: OnlineFeature,
    next_frame: usize,
    chunk: Tensor<f32>,
    context: [i64; CONTEXT_SIZE],
    decoder_out: Vec<f32>,
    tokens: Vec<i64>,
}

/// A final partial chunk needs right context, but padding alone is not model input.
fn chunk_rows(next: usize, ready: usize, finishing: bool) -> Option<std::ops::Range<usize>> {
    let count = ready.saturating_sub(next).min(CHUNK_FRAMES);
    (count == CHUNK_FRAMES || (finishing && count != 0)).then(|| next..next + count)
}

impl Stream<'_> {
    /// Returns the current partial transcript only when new tokens were emitted.
    pub fn push(&mut self, samples: &[f32]) -> EonResult<Option<String>> {
        self.features.accept_waveform(SAMPLE_RATE_HZ, samples);
        let old_len = self.tokens.len();
        self.decode_ready(false)?;
        Ok((self.tokens.len() > old_len).then(|| self.model.detokenize(&self.tokens)))
    }

    pub fn text_token_ids(&self) -> &[i64] {
        &self.tokens
    }

    pub fn finish(mut self) -> EonResult<(String, Vec<i64>)> {
        self.features.input_finished();
        self.decode_ready(true)?;
        Ok((self.model.detokenize(&self.tokens), self.tokens))
    }

    fn decode_ready(&mut self, finishing: bool) -> EonResult<()> {
        let ready = self.features.num_frames_ready();
        while let Some(range) = chunk_rows(self.next_frame, ready, finishing) {
            let rows = &self.features.features[range];
            let (_, chunk) = self.chunk.extract_tensor_mut();
            for (dest, src) in chunk.as_chunks_mut::<NUM_MEL_BINS>().0.iter_mut().zip(rows) {
                dest.copy_from_slice(src);
            }

            // Exactly the old right-context padding, without allocating synthetic feature rows.
            chunk[rows.len() * NUM_MEL_BINS..].fill(0.);
            let encoded = self.run_encoder()?;
            let (shape, values) = encoded.try_extract_tensor::<f32>().map_err(|e| onnx_err("encoder_out", e))?;
            let dim = match &shape[..] {
                [1, frames, width] if *frames > 0 && *width > 0 => *width as usize,
                _ => return Err(onnx_err("encoder_out", "expected [1, frames, width]")),
            };
            for frame in values.chunks_exact(dim) {
                let token = self.model.run_joiner(frame, &self.decoder_out)?;
                if token != BLANK_ID {
                    self.tokens.push(token);
                    self.context = [self.context[1], token];
                    self.decoder_out = self.model.run_decoder(self.context)?;
                }
            }
            // OnlineFeature uses its row count as the absolute frame cursor. Release only the
            // consumed row contents, keeping the overlapping right context for the next chunk.
            for row in &mut self.features.features[self.next_frame..(self.next_frame + CHUNK_SHIFT).min(ready)] {
                *row = Vec::new();
            }
            self.next_frame += CHUNK_SHIFT;
        }
        Ok(())
    }

    fn run_encoder(&mut self) -> EonResult<DynValue> {
        let inputs = &self.model.cache_inputs;
        let mut feed: Vec<(&str, SessionInputValue<'_>)> = Vec::with_capacity(inputs.len() + 1);
        feed.push(("x", (&self.chunk).into()));
        for (i, input) in inputs.iter().enumerate() {
            let value = self.caches.as_ref().map_or(&input.zero, |values| &values[i]);
            feed.push((&input.name, value.into()));
        }

        let mut outputs = self.model.encoder.run(feed).map_err(|e| onnx_err("encoder", e))?;
        let encoder_out = outputs.remove("encoder_out").ok_or_else(|| onnx_err("encoder_out", "missing output"))?;

        let mut caches = Vec::with_capacity(inputs.len());
        for input in inputs {
            let next = outputs
                .remove(&input.output_name)
                .ok_or_else(|| onnx_err("cache", format!("missing {}", input.output_name)))?;
            if next.dtype() != input.zero.dtype() {
                return Err(onnx_err(
                    "cache",
                    format!("{}: expected {}, got {}", input.output_name, input.zero.dtype(), next.dtype()),
                ));
            }
            caches.push(next);
        }
        self.caches = Some(caches);
        Ok(encoder_out)
    }
}

impl Model {
    fn run_decoder(&mut self, context: [i64; CONTEXT_SIZE]) -> EonResult<Vec<f32>> {
        let y = Tensor::from_array(([1usize, CONTEXT_SIZE], context.to_vec())).map_err(|e| onnx_err("decoder", e))?;
        let outputs = self
            .decoder
            .run(ort::inputs!["y" => y])
            .map_err(|e| onnx_err("decoder", e))?;
        let (_, out) = outputs["decoder_out"]
            .try_extract_tensor::<f32>()
            .map_err(|e| onnx_err("decoder_out", e))?;
        Ok(out.to_vec())
    }

    fn run_joiner(&mut self, encoder_frame: &[f32], decoder_out: &[f32]) -> EonResult<i64> {
        let encoder = TensorRef::from_array_view(([1usize, encoder_frame.len()], encoder_frame))
            .map_err(|e| onnx_err("joiner", e))?;
        let decoder = TensorRef::from_array_view(([1usize, decoder_out.len()], decoder_out))
            .map_err(|e| onnx_err("joiner", e))?;
        let outputs = self
            .joiner
            .run(ort::inputs!["encoder_out" => encoder, "decoder_out" => decoder])
            .map_err(|e| onnx_err("joiner", e))?;
        let (_, logits) = outputs["logit"]
            .try_extract_tensor::<f32>()
            .map_err(|e| onnx_err("logit", e))?;
        Ok(argmax(logits))
    }

    fn detokenize(&self, ids: &[i64]) -> String {
        ids.iter()
            .filter_map(|id| self.tokens.get(id))
            .map(String::as_str)
            .collect::<String>()
            .replace(WORD_BOUNDARY, " ")
            .trim()
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flushes_only_chunks_with_real_features_even_at_exact_hop_boundaries() {
        assert_eq!(chunk_rows(0, 0, true), None);
        assert_eq!(chunk_rows(0, CHUNK_FRAMES - 1, false), None);
        assert_eq!(chunk_rows(0, CHUNK_FRAMES, false), Some(0..CHUNK_FRAMES));
        assert_eq!(chunk_rows(CHUNK_SHIFT, CHUNK_FRAMES, true), Some(CHUNK_SHIFT..CHUNK_FRAMES));
        assert_eq!(chunk_rows(CHUNK_SHIFT, CHUNK_SHIFT, true), None);
        assert_eq!(chunk_rows(CHUNK_SHIFT, CHUNK_SHIFT + 1, true), Some(CHUNK_SHIFT..CHUNK_SHIFT + 1));
    }
}
