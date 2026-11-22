//! Peer transport over iroh (docs/implementation/05, transport layer).
//!
//! Peers are dialed by public key over QUIC. Each request runs on its own bidirectional stream and is answered on the
//! same stream. Relays are disabled, so connections go directly between the endpoints and nothing leaves the host
//! unless a peer address points elsewhere. Protocol negotiation uses an ALPN string, so a peer that does not speak it
//! is refused during the handshake.

use std::net::{SocketAddr, SocketAddrV4};

use crate::lan::permits;
use crate::qos::TrafficClass;
use bytes::Bytes;
use eon_primitives::{EonError, EonResult};
use if_addrs::Ifv4Addr;
use iroh::endpoint::{
    presets::Minimal, BindOpts, Builder, Connection, Endpoint, NetReportConfig, PortmapperConfig,
};
use iroh::{EndpointAddr, EndpointId, RelayMode, SecretKey};

/// Application protocol identifier. Peers that do not offer it are refused during the handshake.
pub const ALPN: &[u8] = b"eon/mesh/1";
pub(crate) const PAIRED_ALPN: &[u8] = b"eon/mesh/paired/4";
const LAN_ELECTION_ALPN: &[u8] = b"eon/mesh/lan-election/1";

/// Largest request or reply accepted on one stream. Larger frames are refused rather than buffered.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

fn transport_err(operation: &str, reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: format!("mesh.{operation}"),
        reason: reason.to_string(),
    }
}

pub struct PeerNode {
    endpoint: Endpoint,
    paired: Option<EndpointId>,
    lan: Option<Ifv4Addr>,
}

impl PeerNode {
    /// Binds an endpoint that accepts `alpn` connections and uses no relays.
    pub async fn start_with_alpn(alpn: &[u8]) -> EonResult<Self> {
        Self::bind(Endpoint::builder(Minimal), alpn, None, None).await
    }

    /// Both devices provision each other's public key out of band. An address announcement
    /// alone never authorizes a peer. Reuse the local secret when reconnecting after restart.
    pub async fn start_paired(secret: SecretKey, remote: EndpointId) -> EonResult<Self> {
        Self::bind(Endpoint::builder(Minimal), PAIRED_ALPN, Some(secret), Some(remote)).await
    }

    pub(crate) async fn bind_paired_lan(
        secret: SecretKey,
        remote: EndpointId,
        local: &Ifv4Addr,
    ) -> EonResult<(Self, SocketAddrV4)> {
        let builder = Endpoint::builder(Minimal)
            .clear_ip_transports()
            .bind_addr_with_opts(
                (local.ip, 0),
                BindOpts::default().set_prefix_len(local.prefixlen).set_is_default_route(false),
            )
            .map_err(|e| transport_err("bind", e))?;
        let mut node = Self::bind(builder, LAN_ELECTION_ALPN, Some(secret), Some(remote)).await?;
        node.lan = Some(local.clone());
        match node.endpoint.bound_sockets().as_slice() {
            [SocketAddr::V4(addr)] if *addr.ip() == local.ip => Ok((node, *addr)),
            _ => Err(transport_err("bind", "expected a single LAN socket")),
        }
    }

    async fn bind(builder: Builder, alpn: &[u8], secret: Option<SecretKey>, paired: Option<EndpointId>) -> EonResult<Self> {
        let mut builder = builder
            .alpns(vec![alpn.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .portmapper_config(PortmapperConfig::Disabled)
            .net_report_config(NetReportConfig::minimal());
        if let Some(secret) = secret {
            builder = builder.secret_key(secret);
        }
        let endpoint = builder.bind().await.map_err(|e| transport_err("bind", e))?;
        Ok(Self { endpoint, paired, lan: None })
    }

    pub fn paired_id(&self) -> Option<EndpointId> {
        self.paired
    }

    pub async fn start() -> EonResult<Self> {
        Self::start_with_alpn(ALPN).await
    }

    /// The address other peers use to dial this node.
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Signs this node's current address for peers to store with [`crate::discovery::Directory`].
    pub fn announce(&self, seq: u64) -> EonResult<crate::discovery::Announcement> {
        crate::discovery::sign(self.endpoint.secret_key(), self.addr(), seq)
    }

    /// Sends `payload` to `peer` at bulk priority and returns the reply.
    pub async fn request(&self, peer: impl Into<EndpointAddr>, payload: &[u8]) -> EonResult<Vec<u8>> {
        self.request_with_class(peer, TrafficClass::Bulk, payload).await
    }

    /// Sends `payload` to `peer` on a stream scheduled at `class`'s priority and returns the reply.
    /// `TrafficClass::Audio` is refused because audio travels as datagrams, see [`Link::send_audio`].
    pub async fn request_with_class(
        &self,
        peer: impl Into<EndpointAddr>,
        class: TrafficClass,
        payload: &[u8],
    ) -> EonResult<Vec<u8>> {
        let priority = class
            .stream_priority()
            .ok_or_else(|| transport_err("request", "audio uses datagrams, not streams"))?;
        if payload.len() > MAX_FRAME_BYTES {
            return Err(transport_err("request", "frame too large"));
        }
        let connection = self.dial(peer).await?;
        let (mut send, mut recv) = connection.open_bi().await.map_err(|e| transport_err("open_stream", e))?;
        send.set_priority(priority).map_err(|e| transport_err("set_priority", e))?;
        send.write_all(payload).await.map_err(|e| transport_err("send", e))?;
        send.finish().map_err(|e| transport_err("send", e))?;
        let reply = recv
            .read_to_end(MAX_FRAME_BYTES)
            .await
            .map_err(|e| transport_err("receive", e))?;
        connection.close(0u32.into(), b"done");
        Ok(reply)
    }

    /// Opens a connection to `peer` for datagram audio and classed requests.
    pub async fn connect(&self, peer: impl Into<EndpointAddr>) -> EonResult<Link> {
        Ok(Link {
            connection: self.dial(peer).await?,
        })
    }

    /// Waits for the next incoming connection and returns it as a [`Link`].
    pub async fn accept(&self) -> EonResult<Link> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| transport_err("accept", "endpoint closed"))?;
        if self.lan.as_ref().is_some_and(|lan| !permits(lan, &incoming.remote_addr().into())) {
            incoming.ignore();
            return Err(EonError::SecurityViolation("off-link mesh source".into()));
        }
        let connection = incoming.await.map_err(|e| transport_err("handshake", e))?;
        if self.paired.is_some_and(|id| id != connection.remote_id()) {
            connection.close(403u32.into(), b"unpaired");
            return Err(EonError::SecurityViolation("unpaired mesh peer".into()));
        }
        Ok(Link { connection })
    }

    async fn dial(&self, peer: impl Into<EndpointAddr>) -> EonResult<Connection> {
        let peer = peer.into();
        if self.paired.is_some_and(|id| id != peer.id) {
            return Err(EonError::SecurityViolation("unpaired mesh destination".into()));
        }
        if self.lan.as_ref().is_some_and(|lan| {
            peer.addrs.is_empty() || !peer.addrs.iter().all(|path| permits(lan, path))
        }) {
            return Err(EonError::SecurityViolation("off-link mesh destination".into()));
        }
        let alpn = if self.lan.is_some() {
            LAN_ELECTION_ALPN
        } else if self.paired.is_some() {
            PAIRED_ALPN
        } else {
            ALPN
        };
        self.endpoint
            .connect(peer, alpn)
            .await
            .map_err(|e| transport_err("connect", e))
    }

    /// Accepts one connection, reads one request, answers it with `handler`, and waits for the peer to close.
    pub async fn serve_one(&self, handler: impl FnOnce(Vec<u8>) -> Vec<u8>) -> EonResult<()> {
        self.try_serve_one(|bytes| Ok(handler(bytes))).await
    }

    pub(crate) async fn try_serve_one(&self, handler: impl FnOnce(Vec<u8>) -> EonResult<Vec<u8>>) -> EonResult<()> {
        let Link { connection } = self.accept().await?;
        let (mut send, mut recv) = connection.accept_bi().await.map_err(|e| transport_err("accept_stream", e))?;
        let request = recv
            .read_to_end(MAX_FRAME_BYTES)
            .await
            .map_err(|e| transport_err("receive", e))?;
        let reply = handler(request).inspect_err(|_| connection.close(400u32.into(), b"invalid request"))?;
        if reply.len() > MAX_FRAME_BYTES {
            connection.close(413u32.into(), b"reply too large");
            return Err(transport_err("reply", "frame too large"));
        }
        send.write_all(&reply).await.map_err(|e| transport_err("send", e))?;
        send.finish().map_err(|e| transport_err("send", e))?;
        connection.closed().await;
        Ok(())
    }

    pub async fn close(self) {
        self.endpoint.close().await;
    }
}

/// An established connection to one peer. Audio frames go out as datagrams, so a frame that cannot be delivered in
/// time is dropped rather than retransmitted.
pub struct Link {
    pub(crate) connection: Connection,
}

impl Link {
    /// Sends one audio frame as a datagram. Frames larger than the path allows are refused, not split.
    pub fn send_audio(&self, frame: &[u8]) -> EonResult<()> {
        let max = self
            .connection
            .max_datagram_size()
            .ok_or_else(|| transport_err("send_audio", "peer does not accept datagrams"))?;
        if frame.len() > max {
            return Err(transport_err(
                "send_audio",
                format!("frame of {} bytes exceeds the {max} byte datagram limit", frame.len()),
            ));
        }
        self.connection
            .send_datagram(Bytes::copy_from_slice(frame))
            .map_err(|e| transport_err("send_audio", e))
    }

    /// Waits for the next audio datagram from the peer.
    pub async fn recv_audio(&self) -> EonResult<Vec<u8>> {
        self.connection
            .read_datagram()
            .await
            .map(|frame| frame.to_vec())
            .map_err(|e| transport_err("recv_audio", e))
    }
}
