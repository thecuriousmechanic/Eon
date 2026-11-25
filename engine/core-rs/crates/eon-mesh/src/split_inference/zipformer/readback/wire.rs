//! One bidi stream per utterance. A fresh 16-byte nonce, then frames:
//! kind:u8, count:u8, count * BE u16 Zipformer IDs. Only the final has FIN.
//! Each reply is BLAKE3(nonce || all frames so far), BE u16 caption length,
//! BE u16 pronunciation length, UTF-8 snapshot, then Kokoro u8 IDs (final
//! only). The digest binds order, boundaries and finality, but is NOT proof
//! of correct text or pronunciation: the edge checks both independently.

use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll};

use eon_primitives::{EonResult, id::generate_ulid_u128};
use eon_voice::barge_in::BargeIn;
use iroh::EndpointAddr;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::time::{Instant, timeout, timeout_at};

use super::super::MAX_TEXT;
use super::{
    ALPN, MAX_DELTA, MAX_STEPS, RPC_BUDGET, Source, Step, TokenPeer, Vocabulary, channel_error, invalid, prepare,
    valid_text,
};
use crate::split_inference::REMOTE_BUDGET;

const MORE: u8 = 1;
const FINAL: u8 = 2;
const MAX_PHONEMES: usize = 509;

pub(super) async fn serve(send: &mut SendStream, recv: &mut RecvStream, vocab: &Vocabulary) -> EonResult<()> {
    let mut nonce = [0; 16];
    recv.read_exact(&mut nonce).await.map_err(channel_error)?;
    let mut hash = blake3::Hasher::new();
    hash.update(&nonce);
    let mut pieces = String::new();
    for _ in 0..MAX_STEPS {
        let mut header = [0; 2];
        recv.read_exact(&mut header).await.map_err(channel_error)?;
        let [kind, count] = header;
        if !matches!(kind, MORE | FINAL) || count as usize > MAX_DELTA || (kind == MORE && count == 0) {
            return Err(invalid());
        }
        let mut bytes = vec![0; usize::from(count) * 2];
        recv.read_exact(&mut bytes).await.map_err(channel_error)?;
        hash.update(&header).update(&bytes);
        for &pair in bytes.as_chunks::<2>().0 {
            let id = u16::from_be_bytes(pair);
            let piece = vocab.0.get(&id).filter(|_| id != 0).ok_or_else(invalid)?;
            if pieces.len() + piece.len() > MAX_TEXT + 1 {
                return Err(invalid());
            }
            pieces.push_str(piece);
        }
        let text = pieces.trim();
        valid_text(text)?;
        let pronunciation = if kind == FINAL {
            if recv.read(&mut [0]).await.map_err(channel_error)?.is_some() {
                return Err(invalid());
            }
            let text = text.to_owned();
            tokio::task::spawn_blocking(move || prepare(&text, || false))
                .await
                .map_err(channel_error)??
        } else {
            None
        };
        let phonemes = pronunciation.as_ref().map_or(&[][..], |p| p.ids());
        let mut out = Vec::with_capacity(36 + text.len() + phonemes.len());
        out.extend_from_slice(hash.finalize().as_bytes());
        out.extend_from_slice(&(text.len() as u16).to_be_bytes());
        out.extend_from_slice(&(phonemes.len() as u16).to_be_bytes());
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(phonemes);
        send.write_all(&out).await.map_err(channel_error)?;
        if kind == FINAL {
            return send.finish().map_err(channel_error);
        }
    }
    Err(invalid())
}

fn lost<T>(_: T) -> Source {
    Source::LocalOnChannelError
}

// QUIC's single poll_write either accepts a prefix or registers interest
// without accepting anything. Never hold the submission gate while waiting
// for credit, or let a write_all future resume an unchecked remainder.
async fn submit_fragments(
    signal: &BargeIn,
    mut bytes: &[u8],
    mut write: impl FnMut(&mut Context<'_>, &[u8]) -> Poll<Result<usize, Source>>,
) -> Result<(), Source> {
    while !bytes.is_empty() {
        let n = poll_fn(|cx| {
            signal
                .submit_if_live(|| write(cx, bytes))
                .unwrap_or(Poll::Ready(Err(Source::LocalOnChannelError)))
        })
        .await?;
        if n == 0 {
            return Err(Source::LocalOnChannelError);
        }
        bytes = &bytes[n..];
    }
    Ok(())
}

async fn send_live(signal: &BargeIn, send: &mut SendStream, bytes: &[u8]) -> Result<(), Source> {
    submit_fragments(signal, bytes, |cx, bytes| {
        Pin::new(&mut *send).poll_write(cx, bytes).map_err(lost)
    })
    .await
}

pub(super) struct Link {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    hash: blake3::Hasher,
    deadline: Instant,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.conn.close(0u32.into(), b"done");
    }
}

impl Link {
    pub(super) async fn connect(edge: &TokenPeer, worker: EndpointAddr, signal: &BargeIn) -> Result<Self, Source> {
        timeout(REMOTE_BUDGET, async {
            let conn = edge.0.endpoint.connect(worker, ALPN).await.map_err(lost)?;
            let (mut send, recv) = conn.open_bi().await.map_err(lost)?;
            let nonce = generate_ulid_u128().to_be_bytes();
            send_live(signal, &mut send, &nonce).await?;
            let mut hash = blake3::Hasher::new();
            hash.update(&nonce);
            Ok(Self {
                conn,
                send,
                recv,
                hash,
                deadline: Instant::now() + RPC_BUDGET,
            })
        })
        .await
        .map_err(|_| Source::LocalOnTimeout)?
    }

    pub(super) async fn exchange(&mut self, step: &Step, signal: &BargeIn) -> Result<Vec<u8>, Source> {
        timeout_at(self.deadline.min(Instant::now() + REMOTE_BUDGET), async {
            let mut frame = vec![if step.final_step { FINAL } else { MORE }, step.ids.len() as u8];
            for id in &step.ids {
                frame.extend_from_slice(&id.to_be_bytes());
            }
            self.hash.update(&frame);
            send_live(signal, &mut self.send, &frame).await?;
            if step.final_step {
                signal
                    .submit_if_live(|| self.send.finish())
                    .ok_or(Source::LocalOnChannelError)?
                    .map_err(lost)?;
            }
            let mut header = [0; 36];
            self.recv.read_exact(&mut header).await.map_err(lost)?;
            let text_len = u16::from_be_bytes([header[32], header[33]]) as usize;
            let phones_len = u16::from_be_bytes([header[34], header[35]]) as usize;
            if &header[..32] != self.hash.finalize().as_bytes()
                || text_len != step.text.len()
                || phones_len > MAX_PHONEMES
                || (step.prepared.is_none() && phones_len != 0)
            {
                return Err(Source::LocalOnInvalidReply);
            }
            let mut text = vec![0; text_len];
            self.recv.read_exact(&mut text).await.map_err(lost)?;
            if text != step.text.as_bytes() {
                return Err(Source::LocalOnInvalidReply);
            }
            let mut ids = vec![0; phones_len];
            self.recv.read_exact(&mut ids).await.map_err(lost)?;
            if step.final_step && self.recv.read(&mut [0]).await.map_err(lost)?.is_some() {
                return Err(Source::LocalOnInvalidReply);
            }
            Ok(ids)
        })
        .await
        .map_err(|_| Source::LocalOnTimeout)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::task::Waker;

    #[test]
    fn split_inference_tests_revoked_pending_write_never_submits_its_remainder() {
        let signal = BargeIn::new();
        let has_credit = Cell::new(false);
        let accepted = RefCell::new(Vec::new());
        let mut frame = std::pin::pin!(submit_fragments(&signal, b"abcdef", |_, bytes| {
            if accepted.borrow().is_empty() {
                accepted.borrow_mut().extend_from_slice(&bytes[..2]);
                return Poll::Ready(Ok(2));
            }
            if !has_credit.get() {
                return Poll::Pending;
            }
            accepted.borrow_mut().extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(frame.as_mut().poll(&mut cx).is_pending());
        assert_eq!(&*accepted.borrow(), b"ab"); // already accepted; cannot recall
        signal.trigger();
        has_credit.set(true);
        // Even if select! already picked transport readiness, the write must
        // be fenced again at actual submission, not just before its first poll.
        assert!(matches!(frame.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
        assert_eq!(&*accepted.borrow(), b"ab");
    }
}
