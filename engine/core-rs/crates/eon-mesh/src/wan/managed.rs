//! Native iroh IP has no pre-send ACL for peer-nominated paths. An administrator
//! grants this mode by launching the application in a dedicated, filtered netns.
use super::*;
use futures_lite::StreamExt;
use iroh::{
    address_lookup::{
        AddrFilter, AddressLookup, AddressLookupBuilder, EndpointInfo, N0_DNS_PKARR_RELAY_PROD,
        PkarrPublisher, PkarrResolver,
    },
    dns::{BoxIter, DnsError, DnsResolver, Resolver, TxtRecordData},
};
use n0_watcher::Watcher;
use std::{
    fs,
    net::{Ipv4Addr, Ipv6Addr},
    os::unix::fs::MetadataExt,
    pin::Pin,
};

pub(super) const ALPN: &[u8] = b"eon/mesh/managed-qos/2";

/// Select explicitly on every open. There is no environment-selected
/// staging, DNS TXT, public fallback from a mirror, or implicit publisher.
pub enum N0Provider {
    /// Publish the local ID → home relay association to n0 and look up the
    /// pin there. This discloses presence; closing cannot recall a record.
    Public,
    /// Serve the default pkarr origin and one default relay (NA East) on a
    /// private deployment. Use only literal RFC1918 addresses and this DER CA.
    PrivateMirror {
        relay: Ipv4Addr,
        pkarr: Ipv4Addr,
        root_der: Vec<u8>,
    },
}

impl N0Provider {
    fn grant_name(&self, local: EndpointId, peer: EndpointId) -> String {
        fn digest(data: &[u8]) -> String {
            ring::digest::digest(&ring::digest::SHA256, data)
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        }
        let pair = format!("{local}:{peer}");
        match self {
            Self::Public => format!("eon-n0-public-{}", digest(pair.as_bytes())),
            Self::PrivateMirror {
                relay,
                pkarr,
                root_der,
            } => {
                let policy = format!("{pair}:relay={relay}:pkarr={pkarr}:ca={}", digest(root_der));
                format!("eon-n0-mirror-{}", digest(policy.as_bytes()))
            }
        }
    }
}

fn grant(local: EndpointId, peer: EndpointId, provider: &N0Provider) -> EonResult<()> {
    let denied = || EonError::SecurityViolation("no matching managed WAN namespace grant".into());
    // The supervisor creates this root-owned nsfs bind mount and confines
    // the service before launch. A marker file alone is not a grant.
    let dir = std::path::Path::new("/run/netns");
    let meta = fs::symlink_metadata(dir).map_err(|_| denied())?;
    if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return Err(denied());
    }
    let name = provider.grant_name(local, peer);
    let grant = fs::symlink_metadata(dir.join(name)).map_err(|_| denied())?;
    if !grant.is_file() || grant.uid() != 0 || grant.mode() & 0o022 != 0 {
        return Err(denied());
    }
    for task in fs::read_dir("/proc/self/task").map_err(|_| denied())? {
        let ns = fs::metadata(task.map_err(|_| denied())?.path().join("ns/net"))
            .map_err(|_| denied())?;
        if (grant.dev(), grant.ino()) != (ns.dev(), ns.ino()) {
            return Err(denied());
        }
    }
    Ok(())
}

type Answer<T> = Pin<Box<dyn Future<Output = Result<BoxIter<T>, DnsError>> + Send>>;

#[derive(Clone, Copy, Debug)]
struct MirrorDns {
    pkarr: Ipv4Addr,
    relay: Ipv4Addr,
}

fn answer<T: Send + 'static>(value: Option<T>) -> Answer<T> {
    Box::pin(async { Ok(Box::new(value.into_iter()) as BoxIter<T>) })
}

impl Resolver for MirrorDns {
    fn lookup_ipv4(&self, host: String) -> Answer<Ipv4Addr> {
        let host = host.trim_end_matches('.');
        answer(if host == "dns.iroh.link" {
            Some(self.pkarr)
        } else if host == iroh::defaults::prod::NA_EAST_RELAY_HOSTNAME.trim_end_matches('.') {
            Some(self.relay)
        } else {
            None
        })
    }
    fn lookup_ipv6(&self, _: String) -> Answer<Ipv6Addr> {
        answer(None)
    }
    fn lookup_txt(&self, _: String) -> Answer<TxtRecordData> {
        answer(None)
    }
    fn clear_cache(&self) {}
    fn reset(&self) -> Box<dyn Resolver> {
        Box::new(*self)
    }
}

// Signature verification authenticates a statement, not ownership of an IP.
// Never feed a discovery record's IPs or arbitrary origins to connect().
fn bootstrap(info: &EndpointInfo, pinned: EndpointId) -> EonResult<Option<EndpointAddr>> {
    if info.endpoint_id != pinned {
        return Err(invalid("unexpected discovery identity"));
    }
    let defaults = RelayMode::Default.relay_map();
    let mut addr = EndpointAddr::new(pinned);
    for url in info.relay_urls() {
        if !defaults.contains(url) {
            return Err(invalid("unapproved discovery relay"));
        }
        addr = addr.with_relay_url(url.clone());
    }
    Ok((!addr.addrs.is_empty()).then_some(addr))
}

async fn dial(
    ep: &Endpoint,
    pinned: EndpointId,
    wait: Duration,
) -> EonResult<(Connection, RelayUrl)> {
    // No unfiltered resolver is installed on the endpoint. Every retry
    // re-verifies the peer's signed record and its approved bootstrap origins.
    let lookup = PkarrResolver::builder(
        N0_DNS_PKARR_RELAY_PROD
            .parse()
            .expect("built-in pkarr origin"),
    )
    .into_address_lookup(ep)
    .map_err(transport_error)?;
    let mut backoff = Duration::from_millis(250);
    let mut attempt = (wait / 3).min(Duration::from_secs(3));
    loop {
        let mut records = lookup
            .resolve(pinned)
            .ok_or_else(|| invalid("no pkarr resolver"))?;
        if let Some(Ok(record)) = records.next().await
            && let Some(peer) = bootstrap(record.endpoint_info(), pinned)?
        {
            let relay = peer
                .relay_urls()
                .next()
                .expect("filtered bootstrap relay")
                .clone();
            if let Ok(Ok(conn)) = timeout(attempt, ep.connect(peer, ALPN)).await {
                if conn.remote_id() != pinned || conn.alpn() != ALPN {
                    conn.close(1u8.into(), b"unpaired");
                    return Err(EonError::SecurityViolation("unpaired managed peer".into()));
                }
                return Ok((conn, relay));
            }
            attempt = (attempt * 2).min(Duration::from_secs(10));
        }
        tokio::time::sleep(backoff).await;
        backoff = (2 * backoff).min(Duration::from_secs(2));
    }
}

async fn listen(
    ep: &Endpoint,
    pinned: EndpointId,
    mirror: bool,
) -> EonResult<(Connection, RelayUrl)> {
    let mut homes = ep.home_relay_status();
    let defaults = RelayMode::Default.relay_map();
    let mirror_home = iroh::defaults::prod::default_na_east_relay().url;
    loop {
        let incoming = ep
            .accept()
            .await
            .ok_or_else(|| transport_error("endpoint closed"))?;
        if let IncomingAddr::Relay { url, endpoint_id } = incoming.remote_addr()
            && endpoint_id == pinned
            && defaults.contains(&url)
            && (!mirror || url == mirror_home)
            && homes
                .get()
                .iter()
                .any(|h| h.is_connected() && h.url() == &url)
        {
            let Ok(conn) = incoming.await else { continue };
            if conn.remote_id() == pinned && conn.alpn() == ALPN {
                return Ok((conn, url));
            }
            conn.close(1u8.into(), b"unpaired");
        } else {
            incoming.refuse();
        }
    }
}

impl WanEndpoint {
    /// Opt into ID-only rendezvous using the default iroh relay map, QAD and
    /// signed pkarr service. Both peers must explicitly open within `wait`.
    /// The operator must enforce a default-deny egress ACL for the lifetime
    /// of this process in the pair/provider-specific namespace (Manual 05).
    /// The grant check itself is NOT a firewall. No public discovery or
    /// native UDP is enabled by the other constructors.
    pub async fn open_managed_n0(
        secret: SecretKey,
        pinned: EndpointId,
        provider: N0Provider,
        bytes_per_sec: NonZeroU32,
        wait: Duration,
    ) -> EonResult<Self> {
        if secret.public() == pinned || wait.is_zero() {
            return Err(invalid(
                "expected a distinct managed peer and opening window",
            ));
        }
        grant(secret.public(), pinned, &provider)?;
        // Upstream HTTP clients may honor process-wide proxy settings. They
        // are not part of the grant (nor is an inherited proxy an origin).
        for key in [
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
        ] {
            if std::env::var_os(key).is_some_and(|value| !value.is_empty()) {
                return Err(invalid("managed traversal does not permit a proxy"));
            }
        }
        let origin = || {
            N0_DNS_PKARR_RELAY_PROD
                .parse()
                .expect("built-in pkarr origin")
        };
        let mut report = NetReportConfig::minimal();
        report.https_probes = true; // select a home relay even when UDP is blocked
        let mut builder = Endpoint::builder(Minimal)
            .relay_mode(RelayMode::Default)
            .address_lookup(PkarrPublisher::builder(origin()).addr_filter(AddrFilter::relay_only()))
            .portmapper_config(PortmapperConfig::Disabled)
            .net_report_config(report)
            .transport_config(qos_transport(QuicTransportConfig::builder(), 8))
            .secret_key(secret)
            .alpns(vec![ALPN.to_vec()]);
        let mirror = matches!(provider, N0Provider::PrivateMirror { .. });
        builder = match provider {
            N0Provider::Public => builder.dns_resolver(
                DnsResolver::builder()
                    .with_system_defaults()
                    .disable_fallback()
                    .build(),
            ),
            N0Provider::PrivateMirror {
                pkarr,
                relay,
                root_der,
            } => {
                if !pkarr.is_private() || !relay.is_private() || root_der.is_empty() {
                    return Err(invalid("expected private provider addresses and DER CA"));
                }

                builder
                    .dns_resolver(DnsResolver::custom(MirrorDns { pkarr, relay }))
                    .ca_tls_config(iroh::tls::CaTlsConfig::custom_roots([root_der.into()]))
            }
        };
        timeout(wait, async {
            let ep = builder.bind().await.map_err(transport_error)?;
            ep.online().await;
            // iroh registers these endpoints on their home relays. Elect one
            // dialer so that independently retried dials cannot yield two
            // different application connections. One deadline covers both roles.
            let (conn, relay) = if ep.id() < pinned {
                dial(&ep, pinned, wait).await?
            } else {
                listen(&ep, pinned, mirror).await?
            };
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
        .map_err(|_| transport_error("managed rendezvous timed out"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iroh_and_qos_tests_mirror_grant_binds_the_pair_service_roles_and_ca() {
        let (a, b) = (
            SecretKey::generate().public(),
            SecretKey::generate().public(),
        );
        let x = Ipv4Addr::new(10, 0, 0, 1);
        let y = Ipv4Addr::new(10, 0, 0, 2);
        let mirror = |relay, pkarr, byte| N0Provider::PrivateMirror {
            relay,
            pkarr,
            root_der: vec![byte],
        };
        let expected = mirror(x, y, 1).grant_name(a, b);
        assert_ne!(expected, mirror(y, x, 1).grant_name(a, b));
        assert_ne!(expected, mirror(x, y, 2).grant_name(a, b));
        assert_ne!(expected, mirror(x, y, 1).grant_name(b, a));
        assert_ne!(expected, N0Provider::Public.grant_name(a, b));
    }

    #[test]
    fn iroh_and_qos_tests_managed_bootstrap_uses_only_pinned_default_relays() {
        let pin = SecretKey::generate().public();
        let relay = iroh::defaults::prod::default_na_east_relay().url;
        let ips = EndpointInfo::new(pin).with_ip_addrs(vec!["127.0.0.1:99".parse().unwrap()]);
        assert!(bootstrap(&ips, pin).unwrap().is_none());
        let info = ips.with_relay_url(relay.clone());
        assert_eq!(
            bootstrap(&info, pin).unwrap(),
            Some(EndpointAddr::new(pin).with_relay_url(relay))
        );
        assert!(bootstrap(&info, SecretKey::generate().public()).is_err());
        let mixed = info.with_relay_url("https://unapproved.invalid".parse().unwrap());
        assert!(bootstrap(&mixed, pin).is_err());
    }

    #[tokio::test]
    async fn iroh_and_qos_tests_managed_public_requires_pair_grant_before_bind() {
        let key = SecretKey::generate();
        for peer in [key.public(), SecretKey::generate().public()] {
            let result = WanEndpoint::open_managed_n0(
                key.clone(),
                peer,
                N0Provider::Public,
                NonZeroU32::MIN,
                Duration::from_secs(1),
            )
            .await;
            if peer == key.public() {
                assert!(matches!(result, Err(EonError::ValidationError(_))));
            } else {
                assert!(matches!(result, Err(EonError::SecurityViolation(_))));
            }
        }
    }
}
