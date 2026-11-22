//! One explicit control-stream round trip on an established paired election link.
//! The responder supplies no timestamps, commands or data. No election or playback state changes.

use super::PairedElection;
use crate::qos::TrafficClass;
use eon_primitives::{EonError, EonResult};
use iroh::endpoint::{Connection, ConnectionError, RecvStream, SendStream};
use std::time::{Duration, Instant};
use tokio::time::timeout;

const REQUEST: &[u8] = b"EON-RTT/1?";
const RESPONSE: &[u8] = b"EON-RTT/1!";
const BUDGET: Duration = Duration::from_secs(2);

fn probe_error(reason: &str) -> EonError {
    EonError::OperationFailed {
        operation: "mesh.paired_probe".into(),
        reason: reason.into(),
    }
}

fn unavailable(_: impl std::fmt::Display) -> EonError {
    probe_error("control stream unavailable")
}

/// Explicit fixed-probe service for an already authenticated pair. No task is
/// started; the caller can poll it alongside the independent datagram election.
pub struct ControlResponder(Connection);

impl PairedElection {
    /// Service control streams without borrowing the mutable election state.
    pub fn control_responder(&self) -> ControlResponder {
        ControlResponder(self.link.connection.clone())
    }

    /// Local time from opening a fresh bidirectional stream to receiving the full
    /// fixed reply and its FIN. Excludes mDNS, pairing and QUIC connection setup.
    pub async fn measure_control_rtt(&self) -> EonResult<Duration> {
        let start = Instant::now();
        timeout(BUDGET, async {
            let (mut send, mut receive) =
                self.link.connection.open_bi().await.map_err(unavailable)?;
            send.set_priority(TrafficClass::Control.stream_priority().unwrap())
                .map_err(unavailable)?;
            send.write_all(REQUEST).await.map_err(unavailable)?;
            send.finish().map_err(unavailable)?;
            if receive
                .read_to_end(RESPONSE.len())
                .await
                .map_err(unavailable)?
                != RESPONSE
            {
                return Err(EonError::ValidationError(
                    "invalid paired probe reply".into(),
                ));
            }
            Ok(start.elapsed())
        })
        .await
        .map_err(|_| probe_error("control probe request timed out"))?
    }

    /// Queue the fixed reply and FIN to one exact request. Keep the link open for
    /// the next request or the peer's close; FIN alone is not proof of delivery.
    pub async fn answer_control_probe(&self) -> EonResult<()> {
        self.control_responder().answer_control_probe().await
    }

    /// At the end of a probe session, the reader of the last reply closes first.
    /// Wait once here, not for a (potentially delayed) QUIC ACK after each response.
    pub async fn wait_for_control_peer_close(&self) -> EonResult<()> {
        match timeout(BUDGET, self.link.connection.closed()).await {
            Ok(ConnectionError::ApplicationClosed(close)) if close.error_code == 0u32.into() => {
                Ok(())
            }
            Ok(_) => Err(probe_error("unexpected control peer closure")),
            Err(_) => Err(probe_error("control peer close timed out")),
        }
    }
}

impl ControlResponder {
    /// Concurrent with startup: the first stream may arrive any time up to
    /// `accept_by`. Once accepted, the usual reply budget applies. This doesn't
    /// alter the initiating peer's deadline, which starts before opening the stream.
    pub async fn answer_initial_control_probe(
        &self,
        accept_by: tokio::time::Instant,
    ) -> EonResult<()> {
        let stream = tokio::time::timeout_at(accept_by, self.0.accept_bi())
            .await
            .map_err(|_| probe_error("timed out waiting for first control stream"))?
            .map_err(unavailable)?;
        timeout(BUDGET, Self::reply(stream))
            .await
            .map_err(|_| probe_error("control probe responder timed out"))?
    }

    /// Answer one exact request, without waiting on or changing a local election.
    pub async fn answer_control_probe(&self) -> EonResult<()> {
        timeout(BUDGET, async {
            Self::reply(self.0.accept_bi().await.map_err(unavailable)?).await
        })
        .await
        .map_err(|_| probe_error("control probe responder timed out"))?
    }

    async fn reply((mut send, mut receive): (SendStream, RecvStream)) -> EonResult<()> {
        if receive
            .read_to_end(REQUEST.len())
            .await
            .map_err(unavailable)?
            != REQUEST
        {
            return Err(EonError::ValidationError(
                "invalid paired probe request".into(),
            ));
        }
        send.set_priority(TrafficClass::Control.stream_priority().unwrap())
            .map_err(unavailable)?;
        send.write_all(RESPONSE).await.map_err(unavailable)?;
        send.finish().map_err(unavailable)?;
        Ok(())
    }
}
