//! Linux-only local Qwen2.5-VL-3B-Instruct transcription of one selected still.
//! Opt-in research/evaluation weights; see the upstream Qwen Research License
//! and the two `qwen2.5-vl-3b-*` entries in `scripts/models.lock.json`.
//!
//! Provision the assets explicitly with `scripts/download_models.py --ids
//! qwen2.5-vl-3b-instruct-q4_k_m qwen2.5-vl-3b-mmproj-f16`. From the Cargo
//! workspace, `cargo test --release --locked -p eon-vision --features qwen-vl
//! --test still_vlm` exercises real inference without requiring an ONNX runtime.
//! No weights are bundled. Allow for 3.27 GB of sealed model files **in addition**
//! to the runtime's working memory. Load once and reuse the reader.

use std::fs::File;
use std::io::{Seek, Write};
use std::num::NonZeroU32;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::time::Duration;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{LlamaModel, params::LlamaModelParams};
use llama_cpp_2::mtmd::{
    MtmdBitmap, MtmdContext, MtmdContextParams, MtmdInputChunkType, MtmdInputText,
    mtmd_default_marker,
};
use llama_cpp_2::sampling::LlamaSampler;
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};
use sha2::{Digest, Sha256};

use crate::shared::SharedJpeg;
use crate::{EonError, EonResult, VideoKeyframe, still};

const LANGUAGE: (u64, &str) = (
    1_929_901_056,
    "d02fe9b69ad8cadbbd228e387667af66612c44bed29ffc8eb1e7caf9ac486c12",
);
const PROJECTOR: (u64, &str) = (
    1_338_428_128,
    "b9160fe9d814d1fadf68395677468534778b39ac33c2e7561b7b218626e60d5e",
);
const THREADS: i32 = 8;
const IMAGE_TOKENS: i32 = 384;
const CONTEXT: u32 = 768;
const BATCH: i32 = 256;
const MAX_REPLY: usize = 96;

fn failed(reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: "Qwen2.5-VL selected still".into(),
        reason: reason.to_string(),
    }
}

// llama.cpp opens paths, not byte slices. Hand it only the exact immutable
// bytes that were checked, never a path that could have changed since hashing.
fn verified_copy(source: File, (size, digest): (u64, &str)) -> EonResult<File> {
    let meta = source.metadata().map_err(failed)?;
    if !meta.is_file() || meta.len() != size {
        return Err(EonError::ValidationError("unexpected GGUF size".into()));
    }
    let mut copy = File::from(
        memfd_create(
            c"eon-vl-weights",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .map_err(failed)?,
    );
    let mut buf = vec![0; 128 * 1024];
    let mut offset = 0;
    while offset < size {
        let n = (size - offset).min(buf.len() as u64) as usize;
        source
            .read_exact_at(&mut buf[..n], offset)
            .map_err(failed)?;
        copy.write_all(&buf[..n]).map_err(failed)?;
        offset += n as u64;
    }
    fcntl_add_seals(
        &copy,
        SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
    )
    .map_err(failed)?;
    copy.rewind().map_err(failed)?;
    let mut hasher = Sha256::new();
    let hashed = std::io::copy(&mut copy, &mut hasher).map_err(failed)?;
    if hashed != size || format!("{:x}", hasher.finalize()) != digest {
        return Err(EonError::ValidationError("GGUF digest mismatch".into()));
    }
    Ok(copy)
}

/// Reusable CPU weights, not a chat session. The caller owns the process-wide
/// llama backend. There is no frame source, history, file-name prompt, remote
/// endpoint, Python interpreter or action dispatcher.
pub struct QwenVlReader<'a> {
    // Drop mtmd first: its native context is associated with this text model.
    projector: MtmdContext,
    language: LlamaModel,
    backend: &'a LlamaBackend,
    _weights: [File; 2],
}

impl<'a> QwenVlReader<'a> {
    /// Load the ggml-org Q4_K_M language model and its matching f16 vision
    /// projector. Both checks finish before either native parser sees a file.
    pub fn from_models(
        backend: &'a LlamaBackend,
        language: File,
        projector: File,
    ) -> EonResult<Self> {
        let weights = [
            verified_copy(language, LANGUAGE)?,
            verified_copy(projector, PROJECTOR)?,
        ];
        let fd_path = |file: &File| format!("/proc/self/fd/{}", file.as_raw_fd());
        let language = LlamaModel::load_from_file(
            backend,
            fd_path(&weights[0]),
            &LlamaModelParams::default().with_n_gpu_layers(0),
        )
        .map_err(failed)?;
        let projector = MtmdContext::init_from_file(
            &fd_path(&weights[1]),
            &language,
            &MtmdContextParams {
                use_gpu: false,
                print_timings: false,
                n_threads: THREADS,
                image_min_tokens: 4,
                image_max_tokens: IMAGE_TOKENS,
                ..Default::default()
            },
        )
        .map_err(failed)?;
        if !projector.support_vision() || !projector.decode_use_mrope() {
            return Err(failed("expected a Qwen vision projector with M-RoPE"));
        }
        Ok(Self {
            projector,
            language,
            backend,
            _weights: weights,
        })
    }

    /// Return the model's **untrusted** transcription (the prompt asks for
    /// `NONE` on blank images). A fresh context prevents cross-frame prompting.
    /// The budget covers the whole call; a synchronous encode/prefill can
    /// overrun it before control returns. No partial output on limit or timeout.
    pub fn read_selected(
        &mut self,
        frame: &VideoKeyframe<SharedJpeg>,
        budget: Duration,
    ) -> EonResult<String> {
        let end = still::deadline(budget)?;
        let bytes = frame.jpeg_bytes.read_limited(still::MAX_JPEG)?;
        still::check_time(end)?;
        let image = still::decode(&bytes, Some((frame.width, frame.height)))?;
        still::check_time(end)?;
        let bitmap = MtmdBitmap::from_image_data(frame.width, frame.height, image.as_raw())
            .map_err(failed)?;
        let input = MtmdInputText {
            text: format!(
                "<|im_start|>system\nYou are a helpful assistant.<|im_end|>\n\
                 <|im_start|>user\n{}\nTranscribe all visible text in reading order. \
                 Output only the visible text. If there is no text, output NONE.<|im_end|>\n\
                 <|im_start|>assistant\n",
                mtmd_default_marker()
            ),
            add_special: false,
            parse_special: true,
        };
        let chunks = self.projector.tokenize(input, &[&bitmap]).map_err(failed)?;
        let mut images = (0..chunks.len())
            .filter_map(|i| chunks.get(i))
            .filter(|chunk| chunk.chunk_type() == MtmdInputChunkType::Image);
        if !images
            .next()
            .is_some_and(|chunk| (1..=IMAGE_TOKENS as usize).contains(&chunk.n_tokens()))
            || images.next().is_some()
        {
            return Err(failed(
                "expected one nonempty, bounded image embedding sequence",
            ));
        }
        if chunks.total_tokens() + MAX_REPLY > CONTEXT as usize {
            return Err(failed("selected image exceeds the context budget"));
        }
        let mut ctx = self
            .language
            .new_context(
                self.backend,
                LlamaContextParams::default()
                    .with_n_ctx(NonZeroU32::new(CONTEXT))
                    .with_n_batch(BATCH as u32)
                    .with_n_threads(THREADS)
                    .with_n_threads_batch(THREADS),
            )
            .map_err(failed)?;
        still::check_time(end)?;
        let next_pos = chunks
            .eval_chunks(&mut self.projector, &mut ctx, 0, 0, BATCH, true)
            .map_err(failed)?;
        let mut greedy = LlamaSampler::greedy();
        let mut batch = LlamaBatch::new(1, 1);
        let vocab = self.language.vocab();
        let mut output = Vec::new();
        for pos in (next_pos..).take(MAX_REPLY) {
            still::check_time(end)?;
            let token = greedy.sample(&ctx, -1);
            if vocab.is_eog(token) {
                let text = String::from_utf8(output).map_err(failed)?;
                if text.trim().is_empty() {
                    return Err(failed("empty transcription"));
                }
                still::check_time(end)?;
                return Ok(text);
            }
            vocab.token_to_piece_into(token, &mut output, false, None);
            batch.clear();
            batch.add(token, pos, &[0], true).map_err(failed)?;
            ctx.decode(&mut batch).map_err(failed)?;
        }
        Err(failed("incomplete transcription at output token limit"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_wrong_content_and_load_source_cannot_change_verified_bytes() {
        let mut source = File::from(memfd_create(c"test-gguf", MemfdFlags::CLOEXEC).unwrap());
        source.write_all(b"abc").unwrap();
        assert!(verified_copy(source.try_clone().unwrap(), LANGUAGE).is_err());
        assert!(verified_copy(source.try_clone().unwrap(), (3, LANGUAGE.1)).is_err());
        let copy = verified_copy(
            source.try_clone().unwrap(),
            (
                3,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
        )
        .unwrap();
        source.write_all_at(b"x", 0).unwrap();
        assert!(copy.write_all_at(b"x", 0).is_err());
        let mut bytes = [0; 3];
        copy.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(&bytes, b"abc");
    }
}
