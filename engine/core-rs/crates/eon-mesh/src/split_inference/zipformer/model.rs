use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eon_primitives::{EonError, EonResult};
use eon_voice::asr::ZipformerAsr;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::{MAX_PCM_BYTES, valid_text};

#[derive(Deserialize)]
struct Asset {
    id: String,
    dest: PathBuf,
    sha256: Option<String>,
    size_bytes: Option<u64>,
}

#[derive(Deserialize)]
struct Lock {
    models: Vec<Asset>,
}

pub(super) fn unavailable() -> EonError {
    EonError::CapabilityNotFound {
        capability_id: "pinned Zipformer ASR".into(),
    }
}

/// A preloaded session, shared by at most one inference at a time. No downloads,
/// model selection, runtime paths or session settings are accepted over the wire.
pub struct PinnedZipformer(Arc<Mutex<ZipformerAsr>>);

fn normalized(pcm: &[u8]) -> EonResult<Vec<f32>> {
    let (words, tail) = pcm.as_chunks::<2>();
    if words.is_empty() || pcm.len() > MAX_PCM_BYTES || !tail.is_empty() {
        return Err(EonError::ValidationError("invalid PCM16 utterance".into()));
    }
    Ok(words
        .iter()
        .map(|&v| f32::from(i16::from_le_bytes(v)) / 32768.0)
        .collect())
}

enum Chunk {
    More(Vec<u8>, oneshot::Sender<String>),
    Last(Vec<u8>),
}

pub(super) fn verified_component(dir: &Path, component: &str) -> EonResult<PathBuf> {
    let lock: Lock = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../scripts/models.lock.json"
    )))
    .map_err(|_| unavailable())?;
    let id = format!("zipformer-en-2023-06-26-{component}");
    let asset = lock.models.iter().find(|m| m.id == id).ok_or_else(unavailable)?;
    let path = dir.join(asset.dest.file_name().ok_or_else(unavailable)?);
    let file = File::open(&path).map_err(|_| unavailable())?;
    let size = asset.size_bytes.ok_or_else(unavailable)?;
    let mut digest = Sha256::new();
    if io::copy(&mut file.take(size + 1), &mut digest).map_err(|_| unavailable())? != size
        || format!("{:x}", digest.finalize()) != *asset.sha256.as_ref().ok_or_else(unavailable)?
    {
        return Err(unavailable());
    }
    Ok(path)
}

pub(super) fn verify_model(dir: &Path) -> EonResult<()> {
    for component in ["encoder-int8", "decoder", "joiner-int8", "tokens"] {
        verified_component(dir, component)?;
    }
    Ok(())
}

pub(super) struct Online {
    input: mpsc::Sender<Chunk>,
    result: JoinHandle<EonResult<String>>,
}

impl Online {
    pub(super) async fn push(&self, pcm: Vec<u8>) -> EonResult<String> {
        let (reply, text) = oneshot::channel();
        self.input
            .send(Chunk::More(pcm, reply))
            .await
            .map_err(|_| unavailable())?;
        text.await.map_err(|_| unavailable())
    }

    pub(super) async fn finish(self, pcm: Vec<u8>) -> EonResult<String> {
        self.input.send(Chunk::Last(pcm)).await.map_err(|_| unavailable())?;
        self.result.await.map_err(|_| unavailable())?
    }
}

impl PinnedZipformer {
    /// Blocking startup, not for the audio thread. Use a trusted local model
    /// installation that does not change while models are loaded. The runtime
    /// is the one linked by eon-voice; session initialization failures are errors.
    pub fn load(model_dir: &Path) -> EonResult<Self> {
        verify_model(model_dir)?;
        let model = ZipformerAsr::load(model_dir).map_err(|_| unavailable())?;
        Ok(Self(Arc::new(Mutex::new(model))))
    }

    // Keep the borrowed Zipformer stream and its caches in one bounded CPU job.
    // Cancelling the QUIC request drops its sole sender, waking an idle job.
    pub(super) fn online(&self) -> EonResult<Online> {
        let mut model = self.0.clone().try_lock_owned().map_err(|_| unavailable())?;
        let (input, mut chunks) = mpsc::channel(1);
        let result = tokio::task::spawn_blocking(move || {
            let mut stream = model.start_utterance().map_err(|_| unavailable())?;
            while let Some(chunk) = chunks.blocking_recv() {
                match chunk {
                    Chunk::More(pcm, reply) => {
                        let partial = stream.push(&normalized(&pcm)?).map_err(|_| unavailable())?;
                        let text = partial.map(|p| p.text).unwrap_or_default();
                        valid_text(&text)?;
                        reply.send(text).map_err(|_| unavailable())?;
                    }
                    Chunk::Last(pcm) => {
                        stream.push(&normalized(&pcm)?).map_err(|_| unavailable())?;
                        let text = stream.finish().map_err(|_| unavailable())?;
                        valid_text(&text)?;
                        return Ok(text);
                    }
                }
            }
            Err(unavailable())
        });
        Ok(Online { input, result })
    }

    pub(super) async fn infer(&self, pcm: Vec<u8>) -> EonResult<String> {
        self.online()?.finish(pcm).await
    }
}
