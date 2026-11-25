//! v2: sample count (u32 BE), then fixed half-second PCM chunks, last possibly
//! shorter, with FIN on the last chunk. Each chunk has exactly one response:
//! kind, cumulative PCM BLAKE3, text length (u16 BE), UTF-8 snapshot. An empty
//! step is an acknowledgement with no new words. Only the final is followed by
//! FIN. Lockstep bounds replies to 16 and lets the edge see a preview *before*
//! the rest of the authorized audio has left the edge.

use eon_primitives::{EonError, EonResult};
use iroh::endpoint::{Connection, RecvStream, SendStream};

use super::{MAX_SAMPLES, MAX_TEXT, PinnedZipformer, Preview, Source, channel_error, valid_text};

const CHUNK_BYTES: usize = 2 * 8_000;
const STEP: u8 = 1;
const FINAL: u8 = 2;
const HEADER_BYTES: usize = 1 + 32 + 2;

async fn write_frame(send: &mut SendStream, kind: u8, hash: blake3::Hash, text: &str) -> EonResult<()> {
    // Both bounds and alphabet are enforced by the model before this point.
    let mut frame = Vec::with_capacity(HEADER_BYTES + text.len());
    frame.push(kind);
    frame.extend_from_slice(hash.as_bytes());
    frame.extend_from_slice(&(text.len() as u16).to_be_bytes());
    frame.extend_from_slice(text.as_bytes());
    send.write_all(&frame).await.map_err(channel_error)
}

pub(super) async fn serve(send: &mut SendStream, recv: &mut RecvStream, model: &PinnedZipformer) -> EonResult<()> {
    let invalid = || EonError::ValidationError("invalid PCM16 request".into());
    let mut count = [0; 4];
    recv.read_exact(&mut count).await.map_err(channel_error)?;
    let samples = u32::from_be_bytes(count) as usize;
    if !(1..=MAX_SAMPLES).contains(&samples) {
        return Err(invalid());
    }

    let mut remaining = 2 * samples;
    let mut hash = blake3::Hasher::new();
    let stream = model.online()?;
    loop {
        let mut chunk = vec![0; remaining.min(CHUNK_BYTES)];
        recv.read_exact(&mut chunk).await.map_err(channel_error)?;
        remaining -= chunk.len();
        hash.update(&chunk);
        if remaining == 0 {
            if recv.read(&mut [0]).await.map_err(channel_error)?.is_some() {
                return Err(invalid());
            }
            let text = stream.finish(chunk).await?;
            write_frame(send, FINAL, hash.finalize(), &text).await?;
            return send.finish().map_err(channel_error);
        }
        let text = stream.push(chunk).await?;
        write_frame(send, STEP, hash.finalize(), &text).await?;
    }
}

fn text_len(header: &[u8; HEADER_BYTES], kind: u8, hash: blake3::Hash) -> Result<usize, Source> {
    let len = u16::from_be_bytes([header[33], header[34]]) as usize;
    if header[0] != kind || &header[1..33] != hash.as_bytes() || len > MAX_TEXT {
        return Err(Source::LocalOnInvalidReply);
    }
    Ok(len)
}

fn channel_failure<T>(_: T) -> Source {
    Source::LocalOnChannelError
}

async fn read_frame(recv: &mut RecvStream, kind: u8, hash: blake3::Hash) -> Result<String, Source> {
    let mut header = [0; HEADER_BYTES];
    recv.read_exact(&mut header).await.map_err(channel_failure)?;
    let mut text = vec![0; text_len(&header, kind, hash)?];
    recv.read_exact(&mut text).await.map_err(channel_failure)?;
    let text = String::from_utf8(text).map_err(|_| Source::LocalOnInvalidReply)?;
    valid_text(&text).map_err(|_| Source::LocalOnInvalidReply)?;
    Ok(text)
}

pub(super) async fn request(
    conn: &Connection,
    pcm: &[u8],
    mut preview: impl FnMut(Preview<'_>),
) -> Result<String, Source> {
    let (mut send, mut recv) = conn.open_bi().await.map_err(channel_failure)?;
    let samples = (pcm.len() / 2) as u32;
    send.write_all(&samples.to_be_bytes()).await.map_err(channel_failure)?;
    let mut hash = blake3::Hasher::new();
    let mut sent = 0;
    let mut previous = String::new();
    for chunk in pcm.chunks(CHUNK_BYTES) {
        send.write_all(chunk).await.map_err(channel_failure)?;
        hash.update(chunk);
        sent += chunk.len();
        if sent == pcm.len() {
            send.finish().map_err(channel_failure)?;
            let text = read_frame(&mut recv, FINAL, hash.finalize()).await?;
            if recv.read(&mut [0]).await.map_err(channel_failure)?.is_some() || !text.starts_with(&previous) {
                return Err(Source::LocalOnInvalidReply);
            }
            return Ok(text);
        }
        let text = read_frame(&mut recv, STEP, hash.finalize()).await?;
        if text.is_empty() || text == previous {
            continue;
        }
        // This fixed greedy decoder only appends tokens. Never publish a stale
        // or contradictory snapshot, even from a paired peer.
        if !text.starts_with(&previous) {
            return Err(Source::LocalOnInvalidReply);
        }
        preview(Preview {
            text: &text,
            samples: sent / 2,
        });
        previous = text;
    }
    Err(Source::LocalOnInvalidReply) // empty requests are rejected before dialing
}
