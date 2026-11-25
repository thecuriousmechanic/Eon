//! Offline TaylorAI/bge-micro-v2 (INT8). The two pins are for the artifacts
//! at revision 3edf6d7de0faa426b09780416fe61009f26ae589 in models.lock.json.

use std::{fs, io::Read, path::Path};

use eon_primitives::{EonError, EonResult};
use ort::{session::Session, value::Tensor};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

use super::{Embedding, EmbeddingSpace, UntrustedNote, appraisal::invalid};

struct Artifact {
    name: &'static str,
    size: usize,
    sha256: &'static str,
}

const MODEL: Artifact = Artifact {
    name: "model_quantized.onnx",
    size: 17_409_774,
    sha256: "ed65e36025aa94cb74207dab863c85452919ec0ab7df3512092932aa22c9a33a",
};
const TOKENIZER: Artifact = Artifact {
    name: "tokenizer.json",
    size: 711_661,
    sha256: "cb374d6bc042c22455946f4e09a89d29882a199fdaf8fb25be00dc8b8857a448",
};

impl Artifact {
    // Hash and use the same bounded bytes. Never reopen the path in ORT.
    fn read(&self, dir: &Path) -> EonResult<Vec<u8>> {
        let path = dir.join(self.name);
        let failure = |why: &str| invalid(&format!("BGE {}: {why}", self.name));
        let meta = fs::symlink_metadata(&path).map_err(|_| failure("missing or unreadable"))?;
        if !meta.is_file() || meta.len() != self.size as u64 {
            return Err(failure("expected pinned regular file and length"));
        }
        let file = fs::File::open(path).map_err(|_| failure("unreadable"))?;
        let mut bytes = Vec::with_capacity(self.size);
        file.take(self.size as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| failure("read failed"))?;
        if bytes.len() != self.size || format!("{:x}", Sha256::digest(&bytes)) != self.sha256 {
            return Err(failure("digest mismatch"));
        }
        Ok(bytes)
    }
}

fn runtime_error(error: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: "BGE ONNX".into(),
        reason: error.to_string(),
    }
}

const WIDTH: usize = 384;
const MAX_TOKENS: usize = 512;

/// One reusable CPU session for indexing and querying. No model hub client,
/// custom operators, network fallback, or caller-selected ONNX graph.
pub struct BgeMicroV2 {
    session: Session,
    tokenizer: Tokenizer,
    space: EmbeddingSpace,
}

impl BgeMicroV2 {
    /// `dir` is a trusted, app-controlled local directory, not a document
    /// field, URL, or user-supplied share. Install `bge-micro-v2-int8` and
    /// `bge-micro-v2-tokenizer` there before calling this method.
    pub fn load(dir: impl AsRef<Path>) -> EonResult<Self> {
        let dir = dir.as_ref();
        let mut tokenizer = Tokenizer::from_bytes(TOKENIZER.read(dir)?)
            .map_err(|_| invalid("cannot load pinned BGE tokenizer"))?;
        // The released JSON already specifies right truncation at 512, BERT
        // normalization, WordPiece and CLS/SEP. Like SentenceTransformer's
        // padding=True, pad to longest in batch (no padding for batch size 1).
        tokenizer.with_padding(None);
        let model = MODEL.read(dir)?;
        let session = Session::builder(eon_primitives::onnx::environment()?)
            .map_err(runtime_error)?
            .with_intra_threads(2)
            .map_err(runtime_error)?
            .commit_from_memory(&model)
            .map_err(runtime_error)?;
        let pair = Sha256::digest(format!("{}:{}", MODEL.sha256, TOKENIZER.sha256));
        // Version the representation, not a path or just the hidden width.
        let space =
            EmbeddingSpace::new(&format!("bge-micro-v2:single-mean-l2-v1:{pair:x}"), WIDTH)?;
        Ok(Self {
            session,
            tokenizer,
            space,
        })
    }

    pub fn space(&self) -> &EmbeddingSpace {
        &self.space
    }

    /// Same unprefixed BERT input and attention-masked mean (including
    /// special tokens) for notes and queries. L2-normalize for cosine, then
    /// use the projection's existing per-vector signed int8 representation.
    pub fn embed(&mut self, text: &str) -> EonResult<Embedding> {
        let normalized = self.encode(text)?;
        self.space.quantize(&normalized)
    }

    fn encode(&mut self, text: &str) -> EonResult<[f32; WIDTH]> {
        if text.len() > UntrustedNote::MAX_TEXT_BYTES
            || text.trim().is_empty()
            || text.contains('\0')
        {
            return Err(invalid("missing, oversized, or invalid BGE input"));
        }
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|_| invalid("cannot encode BGE input"))?;
        let mask = encoding.get_attention_mask();
        let seq = encoding.len();
        if !(2..=MAX_TOKENS).contains(&seq) || mask.len() != seq {
            return Err(invalid("unexpected BGE token sequence"));
        }
        let tensor = |ids: &[u32]| {
            Tensor::from_array((
                [1usize, seq],
                ids.iter().map(|&id| i64::from(id)).collect::<Vec<_>>(),
            ))
            .map_err(runtime_error)
        };
        let outputs = self
            .session
            .run(ort::inputs![
                "input_ids" => tensor(encoding.get_ids())?,
                "attention_mask" => tensor(mask)?,
                "token_type_ids" => tensor(encoding.get_type_ids())?,
            ])
            .map_err(runtime_error)?;
        let (shape, hidden) = outputs
            .get("last_hidden_state")
            .ok_or_else(|| invalid("missing BGE last_hidden_state"))?
            .try_extract_tensor::<f32>()
            .map_err(runtime_error)?;
        if shape[..] != [1, seq as i64, WIDTH as i64] {
            return Err(invalid("unexpected BGE hidden state shape"));
        }
        let mut pooled = [0.0; WIDTH];
        let mut count = 0;
        for (&attend, row) in mask.iter().zip(hidden.as_chunks::<WIDTH>().0) {
            if attend != 0 {
                for (sum, &x) in pooled.iter_mut().zip(row) {
                    *sum += x;
                }
                count += 1;
            }
        }
        if count == 0 {
            return Err(invalid("empty BGE attention mask"));
        }
        pooled.iter_mut().for_each(|x| *x /= count as f32);
        let norm = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
        if !norm.is_finite() || norm == 0.0 {
            return Err(invalid("invalid BGE embedding norm"));
        }
        pooled.iter_mut().for_each(|x| *x /= norm);
        Ok(pooled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn released_tokenizer_and_cpu_output_match_reference() {
        let dir = std::env::var_os("EON_BGE_MODEL_DIR").map_or_else(
            || {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../../perception/models/embeddings/bge-micro-v2")
            },
            PathBuf::from,
        );
        let mut model = BgeMicroV2::load(dir).expect("install the pinned BGE graph and tokenizer");
        let text = "HÉLLO, café! 北京 retesting";
        let tokens = model.tokenizer.encode(text, true).unwrap();
        assert_eq!(
            tokens.get_ids(),
            [
                101, 7592, 1010, 7668, 999, 1781, 1755, 2128, 22199, 2075, 102
            ]
        );
        let long = model.tokenizer.encode("café ".repeat(600), true).unwrap();
        assert_eq!(long.len(), 512);
        assert_eq!(long.get_ids()[0], 101);
        assert!(long.get_ids()[1..511].iter().all(|&id| id == 7668));
        assert_eq!(long.get_ids()[511], 102);

        let vector = model.encode(text).unwrap();
        assert!((vector.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-5);
        // Independent Python ORT 1.31 CPU run with the IDs above, all-ones
        // mask and zero segments: normalize(mean(last_hidden_state, axis=1)).
        // Tolerate small differences across CPU INT8 kernels.
        for (i, expected) in [
            (256, -0.17646),
            (341, 0.20729),
            (62, -0.20860),
            (223, -0.25161),
            (319, -0.26678),
        ] {
            assert!(
                (vector[i] - expected).abs() < 0.015,
                "component {i}: {}",
                vector[i]
            );
        }
    }
}
