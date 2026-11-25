//! Opt-in mDNS bootstrap for a single already-paired election. No background dialing.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use eon_primitives::{EonError, EonResult};
use if_addrs::{IfAddr, Ifv4Addr};
use iroh::{EndpointAddr, EndpointId, SecretKey, TransportAddr};
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo, TxtProperty};

use crate::discovery::{Announcement, sign};
use crate::election::PairedElection;
use crate::transport::PeerNode;

const SERVICE: &str = "_eon-mesh._udp.local.";
const TXT: &str = "signed-v1";

fn lan_err(reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: "mesh.lan".into(),
        reason: reason.to_string(),
    }
}

fn interface(selected: Option<Ipv4Addr>) -> EonResult<Ifv4Addr> {
    if_addrs::get_if_addrs()
        .map_err(lan_err)?
        .into_iter()
        .filter(|i| i.is_oper_up() && !i.is_p2p())
        .find_map(|i| match i.addr {
            IfAddr::V4(a) if selected.is_none_or(|ip| a.ip == ip) && private_subnet(&a) => Some(a),
            _ => None,
        })
        .ok_or_else(|| lan_err("no eligible IPv4 LAN interface"))
}

// Keep even iroh's subsequent path probes inside a private, directly attached subnet.
fn private_subnet(a: &Ifv4Addr) -> bool {
    let min = match a.ip.octets() {
        [10, ..] => 8,
        [172, 16..=31, ..] => 12,
        [192, 168, ..] | [169, 254, ..] => 16,
        _ => return false,
    };
    (min..=32).contains(&a.prefixlen) && u32::from(a.netmask) == u32::MAX << (32 - a.prefixlen)
}

pub(crate) fn permits(a: &Ifv4Addr, path: &TransportAddr) -> bool {
    let TransportAddr::Ip(SocketAddr::V4(socket)) = path else {
        return false;
    };
    let mask = u32::from(a.netmask);
    let dest = u32::from(*socket.ip());
    socket.port() != 0
        && dest & mask == u32::from(a.ip) & mask
        && (a.prefixlen >= 31 || (dest & !mask != 0 && dest & !mask != !mask))
}

// The SRV/A records aren't authenticated. Dial only the single signed socket, and
// only if it agrees with DNS-SD and is on this interface. Never pass through extra paths.
fn candidate(
    bytes: &[u8],
    pinned: EndpointId,
    local: &Ifv4Addr,
    port: u16,
    has_address: impl FnOnce(Ipv4Addr) -> bool,
) -> Option<EndpointAddr> {
    let (a, rest) = postcard::take_from_bytes::<Announcement>(bytes).ok()?;
    if !rest.is_empty() || a.id != pinned || a.addr.addrs.len() != 1 {
        return None;
    }
    let path = a.addr.addrs.first()?;
    let TransportAddr::Ip(SocketAddr::V4(socket)) = path else {
        return None;
    };
    if port != socket.port() || !permits(local, path) || !has_address(*socket.ip()) {
        return None;
    }
    a.verify().ok()?;
    Some(a.addr)
}

struct Mdns(ServiceDaemon);

impl Drop for Mdns {
    fn drop(&mut self) {
        // Shutdown also unregisters services. Covers timeout, cancellation and error.
        let _ = self.0.shutdown();
    }
}

fn service(secret: &SecretKey, socket: SocketAddrV4, seq: u64) -> EonResult<ServiceInfo> {
    let addr = EndpointAddr::from_parts(secret.public(), [TransportAddr::Ip(socket.into())]);
    let signed = sign(secret, addr, seq)?.encode()?;
    if TXT.len() + 1 + signed.len() > 255 {
        return Err(lan_err("signed announcement exceeds one DNS-SD TXT string"));
    }
    // Endpoint IDs in hex exceed DNS's 63-byte label limit. This label is not an identity.
    let label = format!("eon-{}", secret.public().fmt_short());
    ServiceInfo::new(
        SERVICE,
        &label,
        &format!("{label}.local."),
        IpAddr::V4(*socket.ip()),
        socket.port(),
        vec![TxtProperty::from((TXT, signed))],
    )
    .map_err(lan_err)
}

/// Establish a dedicated pinned QUIC election link without a remote socket address.
/// Both sides call this with pre-provisioned keys. The higher key advertises a signed
/// address; the lower browses. Discovery stops on success, error or cancellation.
/// The link carries claim metadata and optional fixed control probes, not audio.
///
/// `local_ip` selects a host interface (default: first active private/link-local IPv4,
/// excluding loopback). `seq` is the publisher's announcement sequence. `wait` bounds
/// setup, including a readiness marker from each application on the dedicated
/// `eon/mesh/lan-election/1` protocol. Both LAN peers must support that protocol.
/// The subsequent 15 ms election is unchanged. Keep the returned endpoint with the session.
pub async fn open_election(
    secret: SecretKey,
    pinned: EndpointId,
    local_ip: Option<Ipv4Addr>,
    seq: u64,
    wait: Duration,
) -> EonResult<(PeerNode, PairedElection)> {
    let id = secret.public();
    if id == pinned {
        return Err(EonError::SecurityViolation(
            "LAN election requires a distinct pinned peer".into(),
        ));
    }
    tokio::time::timeout(wait, async {
        let local = interface(local_ip)?;
        let (node, socket) = PeerNode::bind_paired_lan(secret.clone(), pinned, &local).await?;
        let mdns = Mdns(ServiceDaemon::new().map_err(lan_err)?);
        mdns.0.disable_interface(IfKind::All).map_err(lan_err)?;
        mdns.0
            .enable_interface(IpAddr::V4(local.ip))
            .map_err(lan_err)?;
        let election = if id > pinned {
            mdns.0
                .register(service(&secret, socket, seq)?)
                .map_err(lan_err)?;
            loop {
                match PairedElection::accept(&node).await {
                    Err(EonError::SecurityViolation(_)) => continue,
                    result => break result?,
                }
            }
        } else {
            let events = mdns.0.browse(SERVICE).map_err(lan_err)?;
            loop {
                if let ServiceEvent::ServiceResolved(info) =
                    events.recv_async().await.map_err(lan_err)?
                    && let Some(bytes) = info.get_property_val(TXT).flatten()
                    && let Some(addr) = candidate(bytes, pinned, &local, info.port, |ip| {
                        info.get_addresses()
                            .iter()
                            .any(|a| a.to_ip_addr() == IpAddr::V4(ip))
                    })
                {
                    break PairedElection::connect(&node, addr).await?;
                }
            }
        };
        drop(mdns);
        // QUIC connected on this side need not mean the other application has
        // finished discovery. Keep that skew in setup, not in the first turn.
        election.confirm_lan_ready().await?;
        Ok((node, election))
    })
    .await
    .map_err(|_| lan_err("timed out waiting for the pinned LAN peer"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lan() -> Ifv4Addr {
        Ifv4Addr {
            ip: Ipv4Addr::new(192, 168, 1, 5),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            prefixlen: 24,
            broadcast: None,
        }
    }

    #[test]
    fn invalid_or_mismatched_records_cannot_supply_a_dial_target() {
        let secret = SecretKey::generate();
        let id = secret.public();
        let socket = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 7), 5050);
        let info = service(&secret, socket, 1).unwrap();
        let bytes = info.get_property_val(TXT).flatten().unwrap();
        let check = |b: &[u8], peer, port, matches| candidate(b, peer, &lan(), port, |_| matches);
        assert!(check(bytes, id, 5050, true).is_some());
        assert!(check(bytes, SecretKey::generate().public(), 5050, true).is_none());
        assert!(check(bytes, id, 5051, true).is_none());
        assert!(check(bytes, id, 5050, false).is_none());
        let mut tampered = Announcement::decode(bytes).unwrap();
        tampered.seq += 1;
        assert!(check(&tampered.encode().unwrap(), id, 5050, true).is_none());
        let mut trailing = bytes.to_vec();
        trailing.push(0);
        assert!(check(&trailing, id, 5050, true).is_none());
    }

    #[test]
    fn even_a_valid_signature_cannot_add_off_link_or_alternate_paths() {
        let secret = SecretKey::generate();
        let id = secret.public();
        let direct = |addr: &str| TransportAddr::Ip(addr.parse().unwrap());
        let relay = TransportAddr::Relay("https://relay.example.invalid".parse().unwrap());
        for paths in [
            vec![direct("192.168.2.7:5050")],
            vec![direct("192.168.1.255:5050")],
            vec![direct("192.168.1.0:5050")],
            vec![direct("192.168.1.7:0")],
            vec![direct("8.8.8.8:5050")],
            vec![direct("[::1]:5050")],
            vec![relay.clone()],
            vec![direct("192.168.1.7:5050"), relay],
        ] {
            let addr = EndpointAddr::from_parts(id, paths);
            let port = addr.ip_addrs().next().map_or(5050, SocketAddr::port);
            let bytes = sign(&secret, addr, 1).unwrap().encode().unwrap();
            assert!(candidate(&bytes, id, &lan(), port, |_| true).is_none());
        }
        assert!(!private_subnet(&Ifv4Addr {
            prefixlen: 0,
            netmask: Ipv4Addr::UNSPECIFIED,
            ..lan()
        }));
        assert!(!private_subnet(&Ifv4Addr {
            ip: Ipv4Addr::new(8, 8, 8, 8),
            ..lan()
        }));
    }
}
