//! Pinned QUIC for two operator-provisioned identities. Relay-only by default;
//! direct UDP requires separate, explicit private or managed-network provisioning.
//! While open, iroh maintains its connection to the configured relay. The
//! relay sees endpoint IDs, sizes and timing, not the end-to-end QUIC payload.

pub mod continuity;
#[cfg(target_os = "linux")]
mod managed;
mod media;
#[cfg(all(test, target_os = "linux"))]
mod netns;
mod scoped;
#[cfg(test)]
mod tests;
mod wire;

use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use eon_primitives::{EonError, EonResult};
use iroh::endpoint::{
    Builder, Connection, IncomingAddr, NetReportConfig, PathList, PortmapperConfig,
    QuicTransportConfig, QuicTransportConfigBuilder, presets::Minimal,
};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayConfig, RelayMode, RelayUrl, SecretKey};
use prost::Message;
use tokio::time::{Instant, sleep_until, timeout};

use crate::qos::{BulkPacer, TrafficClass};
#[cfg(target_os = "linux")]
pub use managed::N0Provider;
pub use media::AudioCopies;
pub use scoped::{CandidateScope, PrivateWanConfig};
pub use wire::{AudioPcmChunk, CrdtDeltaSync, EpochInvalidationEvent, ReliableFrame};

const ALPN: &[u8] = b"eon/mesh/wan-qos/1";
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);
const RECEIVED: u8 = 1;
const MAX_AUDIO: usize = 1150;
// Includes noq's per-datagram bookkeeping. Fits a maximum wire chunk but
// not two ordinary 20 ms frames (640 PCM bytes each).
const AUDIO_RECV_WINDOW: usize = 1200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arrival {
    Primary,
    PrimaryCopy,
    StandbyCopy,
}

fn transport_error(error: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: "mesh.wan".into(),
        reason: error.to_string(),
    }
}

fn invalid(message: &str) -> EonError {
    EonError::ValidationError(message.into())
}

fn validate_relay(relay: &RelayUrl) -> EonResult<()> {
    let host = relay
        .host_str()
        .ok_or_else(|| invalid("relay must have a host"))?;
    let ip = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .ok();
    if !(relay.scheme() == "https"
        || (relay.scheme() == "http" && ip.is_some_and(|ip| ip.is_loopback())))
        || ip.is_some_and(|ip| {
            ip.is_unspecified() || ip.is_multicast() || ip == std::net::Ipv4Addr::BROADCAST
        })
        || !relay.username().is_empty()
        || relay.password().is_some()
        || relay.path() != "/"
        || relay.port() == Some(0)
        || relay.query().is_some()
        || relay.fragment().is_some()
    {
        return Err(invalid(
            "expected a trusted HTTPS relay origin (HTTP only on literal loopback)",
        ));
    }
    Ok(())
}

/// One pinned application session. By default it uses a single relay and
/// never duplicates audio. Neither announcements nor per-frame addresses
/// enter the dial path.
pub struct WanEndpoint {
    endpoint: Endpoint,
    conn: Connection,
    relay: RelayUrl,
    budget: Budget,
    media: Option<media::MediaLane>,
    // Scoped reconnect destroys the UDP endpoint (including cached paths)
    // before reopening on only the previously trusted relay.
    scoped_root: Option<Vec<u8>>,
}

fn relay_builder(transport: QuicTransportConfigBuilder) -> Builder {
    datagram_builder(transport, 8)
}

fn datagram_builder(transport: QuicTransportConfigBuilder, bidi_streams: u8) -> Builder {
    Endpoint::builder(Minimal)
        .clear_ip_transports()
        .transport_config(qos_transport(transport, bidi_streams))
}

fn qos_transport(transport: QuicTransportConfigBuilder, bidi_streams: u8) -> QuicTransportConfig {
    transport
        .max_concurrent_uni_streams(0u8.into())
        .max_concurrent_bidi_streams(bidi_streams.into())
        // Only Class 0 uses DATAGRAM. Prefer new arrivals to stale queued PCM.
        .datagram_send_buffer_size(MAX_AUDIO)
        .datagram_receive_buffer_size(Some(AUDIO_RECV_WINDOW))
        .build()
}

impl WanEndpoint {
    /// Both peers explicitly call `open` with overlapping `wait` windows.
    /// There is no relay presence before this call; `close` ends the session.
    /// Both operators must provision the same relay and reciprocal keys.
    /// `bytes_per_sec` is the operator's outgoing bandwidth budget, not a probe.
    pub async fn open(
        secret: SecretKey,
        pinned: EndpointId,
        relay: RelayUrl,
        bytes_per_sec: NonZeroU32,
        wait: Duration,
    ) -> EonResult<Self> {
        Self::open_configured(
            secret,
            pinned,
            RelayConfig::new(relay, None),
            bytes_per_sec,
            wait,
            relay_builder(QuicTransportConfig::builder()),
        )
        .await
    }

    /// Like [`Self::open`], but verify the HTTPS relay against only this
    /// operator-provisioned DER trust anchor. No change to peer authentication,
    /// OS trust stores, address lookup, or the relay-only transport policy.
    pub async fn open_with_relay_root(
        secret: SecretKey,
        pinned: EndpointId,
        relay: RelayUrl,
        root_der: Vec<u8>,
        bytes_per_sec: NonZeroU32,
        wait: Duration,
    ) -> EonResult<Self> {
        if relay.scheme() != "https" {
            return Err(invalid("private relay trust anchor requires HTTPS"));
        }
        let builder = relay_builder(QuicTransportConfig::builder())
            .ca_tls_config(iroh::tls::CaTlsConfig::custom_roots([root_der.into()]));
        Self::open_configured(
            secret,
            pinned,
            RelayConfig::new(relay, None),
            bytes_per_sec,
            wait,
            builder,
        )
        .await
    }

    async fn open_configured(
        secret: SecretKey,
        pinned: EndpointId,
        config: RelayConfig,
        bytes_per_sec: NonZeroU32,
        wait: Duration,
        builder: Builder,
    ) -> EonResult<Self> {
        let relay = config.url.clone();
        validate_relay(&relay)?;
        if secret.public() == pinned {
            return Err(EonError::SecurityViolation(
                "WAN peer must have a distinct key".into(),
            ));
        }
        timeout(wait, async {
            let ep = bind(builder, secret, config, ALPN).await?;
            let conn = rendezvous(&ep, pinned, &relay, ALPN).await?;
            Ok(Self {
                endpoint: ep,
                conn,
                relay,
                budget: Budget::new(bytes_per_sec),
                media: None,
                scoped_root: None,
            })
        })
        .await
        .map_err(|_| transport_error("timed out opening the provisioned relay path"))?
    }

    /// Identity authenticated by the current QUIC handshake.
    pub fn peer_id(&self) -> EndpointId {
        self.conn.remote_id()
    }

    /// Passive per-path transport counters for this authenticated connection.
    /// An advertised address or selected path alone is not delivery evidence.
    pub fn paths(&self) -> PathList<'_> {
        self.conn.paths()
    }

    /// Abandon a stalled session and rendezvous once more on the *same* relay.
    /// The pinned peer must explicitly `open` or `reconnect` during `wait`.
    /// This is a new QUIC connection, not migration of streams: outstanding
    /// frames are never replayed, and a missing receipt means delivery is unknown.
    /// The bandwidth budget (including existing bulk reservations) is retained.
    /// After `open_private`, this discards direct consent and returns to relay-only.
    /// A managed N0 session has no fixed provisioned relay; reconnect closes it
    /// and requires a fresh explicit open rather than retaining discovery consent.
    ///
    /// Timeout, error or cancellation ends the session; there is no background retry.
    pub async fn reconnect(mut self, wait: Duration) -> EonResult<Self> {
        #[cfg(target_os = "linux")]
        if self.conn.alpn() == managed::ALPN {
            let _ = timeout(wait, self.close()).await;
            return Err(invalid("managed traversal requires a fresh explicit open"));
        }
        let pinned = self.peer_id();
        self.conn.close(0u8.into(), b"reconnecting");
        let media = self.media.take();
        self.conn = timeout(wait, async {
            let ((), new_primary) = tokio::join!(Self::drain_media(media), async {
                if let Some(root) = self.scoped_root.take() {
                    let secret = self.endpoint.secret_key().clone();
                    self.endpoint.close().await;
                    let builder = relay_builder(QuicTransportConfig::builder())
                        .ca_tls_config(iroh::tls::CaTlsConfig::custom_roots([root.into()]));
                    self.endpoint = bind(
                        builder,
                        secret,
                        RelayConfig::new(self.relay.clone(), None),
                        ALPN,
                    )
                    .await?;
                }
                rendezvous(&self.endpoint, pinned, &self.relay, ALPN).await
            });
            new_primary
        })
        .await
        .map_err(|_| transport_error("timed out reconnecting to the provisioned relay peer"))??;
        Ok(self)
    }

    pub async fn close(self) {
        self.conn.close(0u8.into(), b"session ended");
        tokio::join!(Self::drain_media(self.media), self.endpoint.close());
    }

    async fn drain_media(media: Option<media::MediaLane>) {
        if let Some(media) = media {
            media.close().await;
        }
    }
}

async fn bind(
    builder: Builder,
    secret: SecretKey,
    config: RelayConfig,
    alpn: &[u8],
) -> EonResult<Endpoint> {
    builder
        .clear_address_lookup()
        .portmapper_config(PortmapperConfig::Disabled)
        .net_report_config(NetReportConfig::minimal())
        .relay_mode(RelayMode::Custom(config.into()))
        .secret_key(secret)
        .alpns(vec![alpn.to_vec()])
        .bind()
        .await
        .map_err(transport_error)
}

async fn rendezvous(
    ep: &Endpoint,
    pinned: EndpointId,
    relay: &RelayUrl,
    alpn: &[u8],
) -> EonResult<Connection> {
    // iroh 1.4 chooses a listening "home relay" from probe results. Both
    // peers instead register by dialing. Choose the same connection by key order.
    let peer = EndpointAddr::new(pinned).with_relay_url(relay.clone());
    rendezvous_at(ep, peer, relay, alpn, None).await
}

async fn rendezvous_at(
    ep: &Endpoint,
    peer: EndpointAddr,
    relay: &RelayUrl,
    alpn: &[u8],
    direct: Option<&iroh_base::CustomAddr>,
) -> EonResult<Connection> {
    let pinned = peer.id;
    let dial = async { ep.connect(peer, alpn).await.map_err(transport_error) };
    let (outgoing, incoming) = tokio::try_join!(dial, accept(ep, pinned, relay, direct))?;
    if [&outgoing, &incoming]
        .iter()
        .any(|c| c.remote_id() != pinned || c.alpn() != alpn)
    {
        return Err(EonError::SecurityViolation("unpaired WAN identity".into()));
    }
    Ok(if ep.id() < pinned { outgoing } else { incoming })
}

async fn accept(
    ep: &Endpoint,
    pinned: EndpointId,
    relay: &RelayUrl,
    direct: Option<&iroh_base::CustomAddr>,
) -> EonResult<Connection> {
    loop {
        let incoming = ep
            .accept()
            .await
            .ok_or_else(|| transport_error("endpoint closed"))?;
        if matches!(incoming.remote_addr(), IncomingAddr::Relay { url, endpoint_id }
            if &url == relay && endpoint_id == pinned)
            || matches!(incoming.remote_addr(), IncomingAddr::Custom(addr) if Some(&addr) == direct)
        {
            // The caller also checks the TLS identity, not just relay metadata.
            return incoming.await.map_err(transport_error);
        }
        incoming.refuse();
    }
}

struct Budget {
    start: Instant,
    pacer: Mutex<BulkPacer>,
    // At most one waiter checks the clock. Only reserve once admitted, so a
    // cancelled waiter leaves no debt and observes the current busy window.
    bulk: tokio::sync::Mutex<()>,
}

impl Budget {
    fn new(rate: NonZeroU32) -> Self {
        Self {
            start: Instant::now(),
            pacer: Mutex::new(BulkPacer::new(f64::from(rate.get()))),
            bulk: tokio::sync::Mutex::new(()),
        }
    }

    fn observe(&self, class: TrafficClass) {
        self.pacer
            .lock()
            .unwrap()
            .observe(class, self.start.elapsed());
    }

    async fn pace(&self, size: usize) {
        let _turn = self.bulk.lock().await;
        let at = self.pacer.lock().unwrap().next_bulk_at();
        sleep_until(self.start + at).await;
        self.pacer.lock().unwrap().admit(self.start.elapsed(), size);
    }
}

impl WanEndpoint {
    /// Queue one unreliable PCM chunk. Success is not a delivery receipt:
    /// newer chunks can evict it before transmission, and in-flight chunks
    /// can still arrive late or out of order.
    pub fn send_audio(&self, pcm: &AudioPcmChunk) -> EonResult<()> {
        pcm.validate()?;
        if pcm.encoded_len() > self.conn.max_datagram_size().unwrap_or(0) {
            return Err(invalid("audio exceeds the path datagram size"));
        }

        self.conn
            .send_datagram(Bytes::from(pcm.encode_to_vec()))
            .map_err(transport_error)?;
        self.budget.observe(TrafficClass::Audio);
        Ok(())
    }

    /// Read in arrival order. If the reader stalls, older unread datagrams
    /// are evicted to make room. This is a byte budget, not an age check.
    pub async fn recv_audio(&self) -> EonResult<AudioPcmChunk> {
        Ok(self.recv_audio_observed().await?.0)
    }

    async fn recv_audio_observed(&self) -> EonResult<(AudioPcmChunk, Arrival)> {
        let received = if let Some(media) = &self.media {
            media.recv(&self.conn).await?
        } else {
            let bytes = self.conn.read_datagram().await.map_err(transport_error)?;
            // PCM decoding intentionally ignores the copy ID for compatibility.
            // Preserve the wire mode for callers that verify a capture plan.
            let via = if media::tagged(&bytes) {
                Arrival::PrimaryCopy
            } else {
                Arrival::Primary
            };
            (wire::decode_audio(bytes)?, via)
        };
        self.budget.observe(TrafficClass::Audio);
        Ok(received)
    }

    /// Finishes when the peer has decoded the frame, not when it has persisted
    /// or applied it. One stream per frame, allowing independent class priorities.
    pub async fn send(&self, frame: &ReliableFrame) -> EonResult<()> {
        let (lane, len) = frame.checked_size()?;
        timeout(STREAM_TIMEOUT, async {
            if lane == wire::Lane::Bulk {
                self.budget.pace(len + wire::HEADER).await;
            } else {
                self.budget.observe(lane.class());
            }
            let (mut send, mut recv) = if self.uses_stream_admission() {
                scoped::admission::open(&self.conn, lane.priority()).await?
            } else {
                self.conn.open_bi().await.map_err(transport_error)?
            };
            send.set_priority(lane.priority())
                .map_err(transport_error)?;
            send.write_all(&frame.encode(lane, len))
                .await
                .map_err(transport_error)?;
            send.finish().map_err(transport_error)?;
            if recv.read_to_end(1).await.map_err(transport_error)? != [RECEIVED] {
                return Err(invalid("invalid WAN receipt"));
            }
            Ok(())
        })
        .await
        .map_err(transport_error)?
    }

    pub async fn recv(&self) -> EonResult<ReliableFrame> {
        let first = self.conn.accept_bi().await.map_err(transport_error)?;
        timeout(STREAM_TIMEOUT, async {
            let (mut send, mut recv, header) = if self.uses_stream_admission() {
                scoped::admission::accept(&self.conn, first).await?
            } else {
                let (send, mut recv) = first;
                let mut header = [0; wire::HEADER];
                recv.read_exact(&mut header)
                    .await
                    .map_err(transport_error)?;
                (send, recv, header)
            };
            let (lane, len) = wire::parse_header(header)?;
            let payload = recv.read_to_end(len).await.map_err(transport_error)?;
            if payload.len() != len {
                return Err(invalid("truncated WAN frame"));
            }
            let frame = lane.decode(&payload)?;
            self.budget.observe(lane.class());
            send.set_priority(lane.priority())
                .map_err(transport_error)?;
            send.write_all(&[RECEIVED]).await.map_err(transport_error)?;
            send.finish().map_err(transport_error)?;
            Ok(frame)
        })
        .await
        .map_err(transport_error)?
    }

    fn uses_stream_admission(&self) -> bool {
        #[cfg(target_os = "linux")]
        if self.conn.alpn() == managed::ALPN {
            return true;
        }
        self.scoped_root.is_some()
    }
}
