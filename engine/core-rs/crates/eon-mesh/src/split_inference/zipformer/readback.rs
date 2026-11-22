//! Opt-in spoken dictation readback. The edge runs streaming ASR on one
//! supplied recording. The workstation detokenizes actual joiner-selected
//! text IDs and prepares the final Kokoro pronunciation (not waveform audio).
//! Recomputing the caption and pronunciation locally verifies content, not
//! merely provenance. This is not a speedup claim or an acoustic-codec split.
//!
//! The caller owns a fresh reply renderer and its kernel epoch. Captions are
//! display-only; only a complete checked reply is queued, never a lease,
//! command, model-generated action or speculative acknowledgement.

mod wire;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use eon_primitives::{EonError, EonResult};
use eon_voice::asr::ZipformerAsr;
use eon_voice::barge_in::BargeIn;
use eon_voice::commitment::ReplyRenderer;
use eon_voice::tts::{KokoroFrontend, PreparedClause};
use iroh::{EndpointAddr, EndpointId, SecretKey};
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;

use super::{MAX_SAMPLES, RPC_BUDGET, Source, SpeechPeer, channel_error, model, not_paired, valid_text};

const ALPN: &[u8] = b"eon/readback/zipformer-en-2023-06-26/kokoro-v1/1";
const INPUT_CHUNK: usize = 8_000;
const MAX_STEPS: usize = MAX_SAMPLES.div_ceil(INPUT_CHUNK) + 1;
const MAX_DELTA: usize = 64;

fn invalid() -> EonError {
    EonError::ValidationError("invalid token readback frame".into())
}

fn interrupted() -> EonError {
    EonError::ValidationError("readback was interrupted or already active".into())
}

fn prepare(text: &str, cancelled: impl Fn() -> bool) -> EonResult<Option<PreparedClause>> {
    valid_text(text)?;
    if text.is_empty() {
        return Ok(None);
    }
    // Attribute dictation explicitly. Even an imperative is only being quoted.
    KokoroFrontend::load()?
        .prepare_cancellable(&format!("I heard {}.", text.to_ascii_lowercase()), cancelled)
        .map(Some)
}

/// A separate ALPN with mutual out-of-band key pins. Neither this type nor
/// the grant below exposes the PCM-offload service.
pub struct TokenPeer(SpeechPeer);

impl TokenPeer {
    pub async fn bind(bind: SocketAddr, key: SecretKey, paired: EndpointId) -> EonResult<Self> {
        SpeechPeer::bind_protocol(bind, key, paired, ALPN).await.map(Self)
    }

    pub fn addr(&self) -> EndpointAddr {
        self.0.addr()
    }

    pub async fn close(self) {
        self.0.close().await;
    }

    /// Operator opt-in on the worker: handle one utterance, then stop.
    pub async fn serve_one(&mut self, vocabulary: &Vocabulary) -> EonResult<()> {
        let incoming = self.0.endpoint.accept().await.ok_or_else(|| channel_error("closed"))?;
        timeout(RPC_BUDGET, async {
            let conn = incoming.await.map_err(channel_error)?;
            if conn.remote_id() != self.0.paired {
                conn.close(403u32.into(), b"unpaired");
                return Err(not_paired());
            }
            let (mut send, mut recv) = conn.accept_bi().await.map_err(channel_error)?;
            if let Err(e) = wire::serve(&mut send, &mut recv, vocabulary).await {
                conn.close(400u32.into(), b"incomplete");
                return Err(e);
            }
            conn.closed().await;
            Ok(())
        })
        .await
        .map_err(channel_error)?
    }
}

/// The worker needs only the pinned ASR vocabulary and installed English
/// espeak-ng. No workstation ASR, waveform model, capture or output endpoint.
pub struct Vocabulary(HashMap<u16, String>);

impl Vocabulary {
    pub fn load(dir: &Path) -> EonResult<Self> {
        let path = model::verified_component(dir, "tokens")?;
        let data = std::fs::read_to_string(path).map_err(|_| model::unavailable())?;
        let entries = data
            .lines()
            .map(|line| {
                let (piece, id) = line.rsplit_once(' ').ok_or_else(invalid)?;
                Ok((id.parse().map_err(|_| invalid())?, piece.replace('▁', " ")))
            })
            .collect::<EonResult<_>>()?;
        Ok(Self(entries))
    }
}

struct Step {
    ids: Vec<u16>,
    text: String,
    final_step: bool,
    samples: usize,
    prepared: Option<PreparedClause>,
}

impl Step {
    fn new(
        ids: &[i64],
        text: String,
        final_step: bool,
        samples: usize,
        cancelled: impl Fn() -> bool,
    ) -> EonResult<Self> {
        if ids.len() > MAX_DELTA || ids.contains(&0) {
            return Err(invalid());
        }
        valid_text(&text)?;
        let ids = ids
            .iter()
            .map(|&id| u16::try_from(id).map_err(|_| invalid()))
            .collect::<EonResult<_>>()?;
        let prepared = if final_step { prepare(&text, cancelled)? } else { None };
        Ok(Self {
            ids,
            text,
            final_step,
            samples,
            prepared,
        })
    }
}

pub struct TokenRecognizer(Arc<Mutex<ZipformerAsr>>);

impl TokenRecognizer {
    /// Blocking startup using the digest-verified direct-ORT release graphs.
    pub fn load(dir: &Path) -> EonResult<Self> {
        model::verify_model(dir)?;
        Ok(Self(Arc::new(Mutex::new(ZipformerAsr::load_ort(dir)?))))
    }

    fn start(&self, pcm: &[i16], signal: BargeIn) -> EonResult<mpsc::Receiver<EonResult<Step>>> {
        if pcm.is_empty() || pcm.len() > MAX_SAMPLES {
            return Err(EonError::ValidationError("expected up to 8 seconds of PCM16".into()));
        }
        let mut model = self.0.clone().try_lock_owned().map_err(|_| model::unavailable())?;
        let pcm = pcm.to_vec();
        let (tx, rx) = mpsc::channel(1);
        tokio::task::spawn_blocking(move || {
            let run = || -> EonResult<()> {
                let mut stream = model.start_utterance()?;
                let mut sent = 0;
                let mut samples = 0;
                let mut snapshot = String::new();
                let cancelled = || signal.is_muted() || tx.is_closed();
                for chunk in pcm.chunks(INPUT_CHUNK) {
                    if cancelled() {
                        return Ok(());
                    }
                    samples += chunk.len();
                    let mono: Vec<_> = chunk.iter().map(|&s| f32::from(s) / 32768.).collect();
                    if let Some(partial) = stream.push(&mono)? {
                        snapshot = partial.text;
                    }
                    let ids = stream.text_token_ids().ok_or_else(model::unavailable)?;
                    if ids.len() != sent {
                        let step = Step::new(&ids[sent..], snapshot.clone(), false, samples, cancelled)?;
                        tx.blocking_send(Ok(step)).map_err(|_| model::unavailable())?;
                        sent = ids.len();
                    }
                }
                let (text, ids) = stream.finish_with_text_tokens()?;
                drop(model);
                let final_step = Step::new(&ids[sent..], text, true, samples, cancelled)?;
                tx.blocking_send(Ok(final_step)).map_err(|_| model::unavailable())
            };
            if let Err(e) = run() {
                let _ = tx.blocking_send(Err(e));
            }
        });
        Ok(rx)
    }
}

/// Consumed, bounded input. Pairing never authorizes another recording.
pub enum Readback<'a> {
    LocalOnly(&'a [i16]),
    AllowTokenPeer {
        pcm: &'a [i16],
        edge: &'a TokenPeer,
        worker: EndpointAddr,
    },
}

/// Provisional, replaceable display state. Not a render input.
pub struct Caption<'a> {
    pub text: &'a str,
    pub samples: usize,
    pub new_tokens: usize,
    pub source: &'a Source,
}

impl Readback<'_> {
    /// On any peer fault, discard the response and finish with the *same*
    /// ongoing edge ASR state. No retry, duplicate clause or raw PCM upload.
    /// Does not acquire a lease or open a device; the trusted caller pumps
    /// the renderer on its synthesis worker after a matching kernel event.
    pub async fn queue(
        self,
        local: &TokenRecognizer,
        renderer: &mut ReplyRenderer,
        display: impl FnMut(Caption<'_>),
    ) -> EonResult<super::Transcript> {
        let signal = renderer.playback_signal();
        let mut interruption = signal.watch_interruption().ok_or_else(interrupted)?;
        // The same waker covers dial/open, every stream read/write, and the
        // model channel. Dropping run() closes its link and worker receiver.
        tokio::select! {
            biased;
            () = interruption.wait() => Err(interrupted()),
            result = self.run(local, renderer, display, &signal) => result,
        }
    }

    async fn run(
        self,
        local: &TokenRecognizer,
        renderer: &mut ReplyRenderer,
        mut display: impl FnMut(Caption<'_>),
        signal: &BargeIn,
    ) -> EonResult<super::Transcript> {
        let (pcm, dest) = match self {
            Self::LocalOnly(pcm) => (pcm, None),
            Self::AllowTokenPeer { pcm, edge, worker } => {
                if edge.0.paired != worker.id {
                    return Err(not_paired());
                }
                (pcm, Some((edge, worker)))
            }
        };
        let check_live = || (!signal.is_muted()).then_some(()).ok_or_else(interrupted);
        check_live()?;
        let mut steps = local.start(pcm, signal.clone())?;
        let (mut link, mut source) = match dest {
            Some((edge, worker)) => match wire::Link::connect(edge, worker, signal).await {
                Ok(link) => (Some(link), Source::PairedWorker),
                Err(source) => (None, source),
            },
            None => (None, Source::LocalOnly),
        };
        while let Some(step) = timeout(RPC_BUDGET, steps.recv())
            .await
            .map_err(|_| model::unavailable())?
        {
            let mut step = step?;
            check_live()?;
            if let Some(remote) = &mut link {
                match remote.exchange(&step, signal).await {
                    Ok(ids) => {
                        if let Some(clause) = &mut step.prepared
                            && !clause.accept_verified_ids(ids)
                        {
                            source = Source::LocalOnInvalidReply;
                            link = None;
                        }
                    }

                    Err(reason) => {
                        link = None;
                        source = reason;
                    }
                }
            }
            check_live()?;
            if step.final_step {
                if let Some(clause) = step.prepared {
                    renderer.queue_prepared(clause)?;
                }
                return Ok(super::Transcript {
                    text: step.text,
                    source,
                });
            }
            display(Caption {
                text: &step.text,
                samples: step.samples,
                new_tokens: step.ids.len(),
                source: &source,
            });
        }
        Err(model::unavailable())
    }
}
