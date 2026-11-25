//! One explicitly supplied in-memory utterance per mutually pinned QUIC stream.
//! The worker runs a single incremental Zipformer decoder on mono 16 kHz PCM16LE.
//! Previews bind to PCM prefixes; only the final binds to the whole recording.
//! Digests do not prove the worker computed the right words. Nothing here
//! captures or plays audio, interprets commands, or dispatches reflex hints.

mod model;
mod wire;
pub use model::PinnedZipformer;

#[cfg(feature = "token-readback")]
pub mod readback;

use std::net::SocketAddr;
use std::time::Duration;

use eon_primitives::{EonError, EonResult};
use iroh::endpoint::{Endpoint, presets::Minimal};
use iroh::{EndpointAddr, EndpointId, RelayMode, SecretKey};
use tokio::time::timeout;

const ALPN: &[u8] = b"eon/asr/zipformer-en-2023-06-26/2";
pub const MAX_SAMPLES: usize = 8 * 16_000;
const MAX_PCM_BYTES: usize = 2 * MAX_SAMPLES;
const MAX_TEXT: usize = 2048;
const DIAL_BUDGET: Duration = Duration::from_secs(2);
const RPC_BUDGET: Duration = Duration::from_secs(8);

fn channel_error(_: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: "mesh.asr".into(),
        reason: "speech channel unavailable".into(),
    }
}

fn not_paired() -> EonError {
    EonError::SecurityViolation("expected the pinned speech peer".into())
}

fn valid_text(text: &str) -> EonResult<()> {
    if text.len() > MAX_TEXT
        || text.trim() != text
        || !text
            .bytes()
            .all(|b| b.is_ascii_uppercase() || matches!(b, b' ' | b'\''))
    {
        return Err(EonError::ValidationError("invalid English ASR result".into()));
    }
    Ok(())
}

/// Dedicated service (no relays or discovery), mutually pinned with the same
/// out-of-band keys as the mesh. It cannot dispatch arbitrary mesh requests.
pub struct SpeechPeer {
    endpoint: Endpoint,
    paired: EndpointId,
}

impl SpeechPeer {
    pub async fn bind(bind: SocketAddr, key: SecretKey, paired: EndpointId) -> EonResult<Self> {
        Self::bind_protocol(bind, key, paired, ALPN).await
    }

    async fn bind_protocol(bind: SocketAddr, key: SecretKey, paired: EndpointId, alpn: &[u8]) -> EonResult<Self> {
        if key.public() == paired {
            return Err(not_paired());
        }
        let endpoint = Endpoint::builder(Minimal)
            .secret_key(key)
            .alpns(vec![alpn.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr(bind)
            .map_err(channel_error)?
            .bind()
            .await
            .map_err(channel_error)?;
        Ok(Self { endpoint, paired })
    }

    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    pub async fn close(self) {
        self.endpoint.close().await;
    }

    /// Wait for one request. Once an incoming connection arrives, handshake,
    /// reading, inference and sending the reply share a single deadline.
    pub async fn serve_one(&mut self, model: Option<&PinnedZipformer>) -> EonResult<()> {
        let incoming = self.endpoint.accept().await.ok_or_else(|| channel_error("closed"))?;
        timeout(RPC_BUDGET, async {
            let conn = incoming.await.map_err(channel_error)?;
            if conn.remote_id() != self.paired {
                conn.close(403u32.into(), b"unpaired");
                return Err(not_paired());
            }
            let Some(model) = model else {
                conn.close(503u32.into(), b"unavailable");
                return Err(model::unavailable());
            };
            let (mut send, mut recv) = conn.accept_bi().await.map_err(channel_error)?;
            if let Err(err) = wire::serve(&mut send, &mut recv, model).await {
                conn.close(400u32.into(), b"incomplete");
                return Err(err);
            }
            conn.closed().await;
            Ok(())
        })
        .await
        .map_err(channel_error)?
    }
}

/// The caller supplies one consent decision bound to the exact samples. Pairing
/// alone never opts a subsequent utterance in. `LocalOnly` does not need a peer.
pub enum Utterance<'a> {
    LocalOnly(&'a [i16]),
    AllowPairedWorker {
        pcm: &'a [i16],
        edge: &'a SpeechPeer,
        worker: EndpointAddr,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    PairedWorker,
    LocalOnly,
    LocalOnChannelError,
    LocalOnTimeout,
    LocalOnInvalidReply,
}

// Intentionally no Debug implementation containing transcript text.
pub struct Transcript {
    pub text: String,
    pub source: Source,
}

/// Display-only snapshot from the worker, not a final result or a command.
/// `samples` is the length of the digest-checked prefix, not a timestamp.
pub struct Preview<'a> {
    pub text: &'a str,
    pub samples: usize,
}

async fn remote(
    edge: &SpeechPeer,
    worker: EndpointAddr,
    pcm: &[u8],
    preview: impl FnMut(Preview<'_>),
) -> Result<String, Source> {
    let conn = timeout(DIAL_BUDGET, edge.endpoint.connect(worker, ALPN))
        .await
        .map_err(|_| Source::LocalOnTimeout)?
        .map_err(|_| Source::LocalOnChannelError)?;
    // connect authenticates worker.id, which the consent check matched to edge.paired.
    let reply = timeout(RPC_BUDGET, wire::request(&conn, pcm, preview)).await;
    conn.close(0u32.into(), b"done");
    reply.map_err(|_| Source::LocalOnTimeout)?
}

impl Utterance<'_> {
    /// No local inference on success. A channel failure or invalid reply runs
    /// the original PCM through the local model; no model means an explicit error.
    pub async fn transcribe(self, local: Option<&PinnedZipformer>) -> EonResult<Transcript> {
        self.transcribe_with_previews(local, |_| {}).await
    }

    /// Calls a nonblocking display observer before uploading the next chunk.
    /// Previews are provisional: discard them on return, error or cancellation.
    /// Only the returned `Transcript` is final. Local inference produces no previews.
    pub async fn transcribe_with_previews(
        self,
        local: Option<&PinnedZipformer>,
        preview: impl FnMut(Preview<'_>),
    ) -> EonResult<Transcript> {
        let (samples, dest) = match self {
            Self::LocalOnly(samples) => (samples, None),
            Self::AllowPairedWorker { pcm, edge, worker } => {
                if edge.paired != worker.id {
                    return Err(not_paired());
                }
                (pcm, Some((edge, worker)))
            }
        };
        if samples.is_empty() || samples.len() > MAX_SAMPLES {
            return Err(EonError::ValidationError("expected up to 8 seconds of PCM16".into()));
        }
        let pcm: Vec<_> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let source = if let Some((edge, worker)) = dest {
            match remote(edge, worker, &pcm, preview).await {
                Ok(text) => {
                    return Ok(Transcript {
                        text,
                        source: Source::PairedWorker,
                    });
                }
                Err(reason) => reason,
            }
        } else {
            Source::LocalOnly
        };
        let text = timeout(RPC_BUDGET, local.ok_or_else(model::unavailable)?.infer(pcm))
            .await
            .map_err(|_| model::unavailable())??;
        Ok(Transcript { text, source })
    }
}
