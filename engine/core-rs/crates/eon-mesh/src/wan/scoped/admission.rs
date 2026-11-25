//! Hand an application frame to QUIC exactly once. A stream created during
//! a silent cut can have its first bytes stranded in the abandoned path's
//! retransmission window. Confirm the empty offer before writing any data.
use super::*;
use futures_lite::StreamExt;
use iroh::endpoint::{ReadError, ReadExactError, RecvStream, SendStream, WriteError};

type Stream = (SendStream, RecvStream);
const OFFER: u8 = 0xa1;
const READY: u8 = 0xa2;
const SUPERSEDED: u8 = 0xa3;

pub(in crate::wan) async fn open(conn: &Connection, priority: i32) -> EonResult<Stream> {
    let mut paths = conn.paths_stream();
    let mut was_direct = paths.next().await.is_some_and(|ps| {
        ps.iter()
            .any(|p| p.is_selected() && (p.remote_addr().is_custom() || p.remote_addr().is_ip()))
    });
    loop {
        let (mut tx, mut rx) = conn.open_bi().await.map_err(transport_error)?;
        tx.set_priority(priority).map_err(transport_error)?;
        let offer = async {
            tx.write_all(&[OFFER]).await.map_err(transport_error)?;
            let mut reply = [0];
            rx.read_exact(&mut reply).await.map_err(transport_error)?;
            if reply != [READY] {
                return Err(invalid("bad scoped stream admission"));
            }
            Ok(())
        };
        tokio::select! {
            result = offer => { result?; return Ok((tx, rx)); }
            result = async {
                while let Some(ps) = paths.next().await {
                    if ps.iter().any(|p| p.is_selected() && p.remote_addr().is_relay()) { return Ok(()); }
                }
                Err(transport_error("connection closed during stream admission"))
            }, if was_direct => {
                result?;
                // This stream never carried an application payload. Retry the
                // offer once, on this same connection, on actual QUIC fallback.
                let _ = tx.reset(SUPERSEDED.into());
                let _ = rx.stop(SUPERSEDED.into());
                was_direct = false;
            }
        }
    }
}

fn superseded(err: &ReadExactError) -> bool {
    matches!(err, ReadExactError::ReadError(ReadError::Reset(code)) if *code == SUPERSEDED.into())
}

pub(in crate::wan) async fn accept(
    conn: &Connection,
    mut stream: Stream,
) -> EonResult<(SendStream, RecvStream, [u8; wire::HEADER])> {
    loop {
        let (mut tx, mut rx) = stream;
        let mut offer = [0];
        let mut header = [0; wire::HEADER];
        match rx.read_exact(&mut offer).await {
            Err(e) if superseded(&e) => {}
            Err(e) => return Err(transport_error(e)),
            Ok(()) if offer != [OFFER] => return Err(invalid("invalid scoped stream offer")),
            Ok(()) => match tx.write_all(&[READY]).await {
                Err(WriteError::Stopped(code)) if code == SUPERSEDED.into() => {}
                Err(e) => return Err(transport_error(e)),
                Ok(()) => match rx.read_exact(&mut header).await {
                    Ok(()) => return Ok((tx, rx, header)),
                    Err(e) if superseded(&e) => {}
                    Err(e) => return Err(transport_error(e)),
                },
            },
        }
        stream = conn.accept_bi().await.map_err(transport_error)?;
    }
}
