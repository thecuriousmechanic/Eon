//! Private paired Binding discovery and iroh custom UDP transport. There is
//! deliberately no native iroh IP sender on this endpoint.
pub(super) mod admission;
mod binding;
pub(super) mod udp;

use super::*;
use iroh::TransportAddr;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::ops::RangeInclusive;
use std::sync::Arc;
use tokio::net::UdpSocket;

const SCOPED_ALPN: &[u8] = b"eon/mesh/private-qos/1";

/// A dedicated, operator-owned RFC1918 unicast pool. Provision *bounds*, not
/// the live peer address. A scope is at most a /24 and 256 unprivileged ports.
pub struct CandidateScope {
    network: Ipv4Addr,
    mask: u32,
    ports: RangeInclusive<u16>,
}

impl CandidateScope {
    pub fn new(network: Ipv4Addr, prefix: u8, ports: RangeInclusive<u16>) -> EonResult<Self> {
        if !network.is_private()
            || !(24..=32).contains(&prefix)
            || ports.is_empty()
            || *ports.start() < 1024
            || ports.end() - ports.start() >= 256
        {
            return Err(invalid("invalid private UDP candidate pool"));
        }
        let mask = u32::MAX << (32 - prefix);
        if u32::from(network) & !mask != 0 {
            return Err(invalid("expected the canonical candidate network"));
        }
        Ok(Self {
            network,
            mask,
            ports,
        })
    }

    fn contains(&self, addr: SocketAddrV4) -> bool {
        let ip = u32::from(*addr.ip());
        let host = ip & !self.mask;
        ip & self.mask == u32::from(self.network)
            && (self.mask >= u32::MAX - 1 || (host != 0 && host != !self.mask))
            && self.ports.contains(&addr.port())
    }
}

/// Provision out of band with the two pins. The private STUN origin is also
/// the trusted pair publisher (see the WAN guide); a generic Binding server
/// cannot tell us where the *peer* is. No peer-supplied URL, DNS or IP list.
pub struct PrivateWanConfig {
    pub relay: RelayUrl,
    pub relay_root_der: Vec<u8>,
    pub bind: SocketAddrV4,
    pub stun: SocketAddrV4,
    pub stun_username: String,
    pub stun_password: Vec<u8>,
    pub candidates: CandidateScope,
}

impl PrivateWanConfig {
    fn relay_ip(&self) -> EonResult<Ipv4Addr> {
        validate_relay(&self.relay)?;
        self.relay
            .host_str()
            .and_then(|h| h.parse::<Ipv4Addr>().ok())
            .filter(|ip| {
                ip.is_private() && self.relay.scheme() == "https" && !self.relay_root_der.is_empty()
            })
            .ok_or_else(|| invalid("expected a private HTTPS IPv4 origin and DER trust anchor"))
    }

    fn validate(&self) -> EonResult<()> {
        self.relay_ip()?;
        if !self.bind.ip().is_private()
            || !self.stun.ip().is_private()
            || self.stun.port() == 0
            || self.bind.ip() == self.stun.ip()
            || !(1..=64).contains(&self.stun_username.len())
            || !self
                .stun_username
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            || !(32..=128).contains(&self.stun_password.len())
        {
            return Err(invalid("invalid private STUN provisioning"));
        }
        Ok(())
    }

    fn admit(&self, own: SocketAddrV4, peer: SocketAddrV4) -> EonResult<()> {
        let relay = self.relay_ip()?;
        if own == peer
            || [own, peer].iter().any(|a| {
                !self.candidates.contains(*a) || a.ip() == self.stun.ip() || *a.ip() == relay
            })
            || peer.ip() == self.bind.ip()
        {
            return Err(invalid(
                "refused out-of-scope or reflected private candidate",
            ));
        }
        Ok(())
    }
}

impl WanEndpoint {
    /// Opt in to private direct QUIC with the provisioned relay as backup.
    /// If UDP or paired Binding is unavailable, dial only the relay. Invalid
    /// authenticated candidates fail closed. A new open performs new discovery;
    /// the socket is never released between STUN and QUIC.
    pub async fn open_private(
        secret: SecretKey,
        pinned: EndpointId,
        config: PrivateWanConfig,
        bytes_per_sec: NonZeroU32,
        wait: Duration,
    ) -> EonResult<Self> {
        config.validate()?;
        if pinned == secret.public() {
            return Err(invalid("WAN peer must have a distinct key"));
        }
        // The relay needs time for TLS, QUIC and reciprocal peer registration,
        // including a lost first dial. Spend at most a quarter on optional UDP.
        let discovery_end = Instant::now() + (wait / 4).min(Duration::from_millis(1750));
        timeout(wait, async {
            let mut builder = relay_builder(QuicTransportConfig::builder()).ca_tls_config(
                iroh::tls::CaTlsConfig::custom_roots([config.relay_root_der.clone().into()]),
            );
            let mut dst = EndpointAddr::new(pinned).with_relay_url(config.relay.clone());
            let mut custom = None;
            let candidate = tokio::time::timeout_at(discovery_end, async {
                let Ok(socket) = UdpSocket::bind(config.bind).await else {
                    return Ok(None);
                };
                let mapping = binding::discover(&socket, &config, pinned, discovery_end).await?;
                Ok::<_, EonError>(mapping.map(|pair| (socket, pair)))
            })
            .await
            .unwrap_or(Ok(None))?;
            if let Some((socket, (own, peer))) = candidate {
                config.admit(own, peer)?;
                // Connected UDP: the kernel also enforces the sole validated
                // destination/source; no peer wire address reaches send_to.
                if socket.connect(peer).await.is_ok() {
                    let addr = udp::address(peer);
                    dst.addrs.insert(TransportAddr::Custom(addr.clone()));
                    custom = Some(addr);
                    builder =
                        builder.add_custom_transport(Arc::new(udp::Direct::new(socket, own, peer)));
                }
            }
            let relay = config.relay;
            let ep = bind(
                builder,
                secret,
                RelayConfig::new(relay.clone(), None),
                SCOPED_ALPN,
            )
            .await?;
            let conn = rendezvous_at(&ep, dst, &relay, SCOPED_ALPN, custom.as_ref()).await?;
            Ok(Self {
                endpoint: ep,
                conn,
                relay,
                budget: Budget::new(bytes_per_sec),
                media: None,
                scoped_root: Some(config.relay_root_der),
            })
        })
        .await
        .map_err(|_| transport_error("scoped WAN rendezvous timed out"))?
    }
}
