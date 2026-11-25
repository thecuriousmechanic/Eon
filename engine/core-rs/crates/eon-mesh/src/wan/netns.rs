//! Linux-only roles for scripts/test_wan_nat.sh. No target arguments.
//! Bare QAD uses test-only native IP; the STUN and N0 profiles use shipping APIs.
use super::*;
use crate::sync::Replica;
use iroh::TransportAddr;
use iroh_relay::{
    RelayQuicConfig,
    server::{
        Access, AccessControl, CertConfig, ClientRequest, QuicConfig, Server, ServerConfig,
        TlsConfig,
    },
};
use std::{fs, net::Ipv4Addr, os::unix::fs::MetadataExt, path::Path, sync::Arc};
use tokio::time::{MissedTickBehavior, interval_at, sleep};

mod field;

const IP: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);
const URL: &str = "https://10.77.0.1:8443";
const MEDIA_URL: &str = "https://10.77.0.1:8444";
const WAIT: Duration = Duration::from_secs(25);
const CHUNK: usize = 64 * 1024;

fn mark(name: &str) {
    fs::write(name, []).unwrap();
}

async fn wait_for(name: &str, deadline: Duration) {
    timeout(deadline, async {
        while !Path::new(name).exists() {
            assert!(
                !Path::new("abort").exists(),
                "fixture stopped while waiting for {name}"
            );
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing marker {name}"));
}

async fn wait(name: &str) {
    wait_for(
        name,
        if name == "fallback-a" {
            Duration::from_secs(65)
        } else {
            WAIT
        },
    )
    .await;
}

fn pin(side: &str) -> EndpointId {
    fs::read_to_string(format!("keys/{side}.pub"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[derive(Debug)]
struct Members([EndpointId; 2]);
impl AccessControl for Members {
    async fn on_connect(&self, req: &ClientRequest) -> Access {
        if self.0.contains(&req.endpoint_id()) {
            Access::Allow
        } else {
            Access::Deny { reason: None }
        }
    }
}

fn media_enabled() -> bool {
    match std::env::var("EON_WAN_MEDIA").as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => panic!("missing fixture media policy"),
    }
}

async fn start_relay(port: u16, qad: bool) -> Server {
    let tls = rustls::ServerConfig::builder_with_provider(iroh::tls::default_provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![fs::read("keys/leaf.der").unwrap().into()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(fs::read("keys/leaf.pk8").unwrap()).into(),
        )
        .unwrap();
    let mut relay = iroh_relay::server::RelayConfig::new((Ipv4Addr::LOCALHOST, 0));
    relay.tls = Some(TlsConfig::new(
        (IP, port),
        CertConfig::Manual { server_config: tls },
    ));
    relay.access = Arc::new(Members([pin("a"), pin("b")]));
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(relay);
    cfg.quic = qad.then(|| QuicConfig::new((IP, 7842)));
    Server::spawn(cfg).await.unwrap()
}

fn private_stun() -> bool {
    match std::env::var("EON_WAN_DISCOVERY").as_deref() {
        Ok("qad" | "n0") => false,
        Ok("stun") => true,
        _ => panic!("missing fixture discovery policy"),
    }
}

fn isolated_n0() -> bool {
    std::env::var("EON_WAN_DISCOVERY").as_deref() == Ok("n0")
}

async fn relay(field: bool) {
    let server = start_relay(8443, !private_stun()).await;
    let media = if media_enabled() {
        Some(start_relay(8444, false).await)
    } else {
        None
    };
    println!(
        "relay HTTPS={:?} QAD={:?}",
        server.https_addr(),
        server.quic_addr()
    );
    mark("ready");
    if field {
        wait_for("cut", Duration::from_secs(60)).await;
        let m = &server.metrics().server;
        let (rx, tx) = (m.bytes_recv.get(), m.bytes_sent.get());
        wait_for("finished", Duration::from_secs(50)).await;
        let (rx, tx) = (m.bytes_recv.get() - rx, m.bytes_sent.get() - tx);
        println!("relay after interface removal: forwarded_rx={rx} forwarded_tx={tx}");
        assert!(rx >= CHUNK as u64 && tx >= CHUNK as u64);
        if let Some(media) = media {
            media.shutdown().await.unwrap();
        }
        server.shutdown().await.unwrap();
        return;
    }
    wait("fallback-a").await;
    let m = &server.metrics().server;
    let (rx, tx) = (m.bytes_recv.get(), m.bytes_sent.get());
    fs::write("counter-ready", monotonic_ns().to_string()).unwrap();
    wait("finished").await;
    let (rx, tx) = (m.bytes_recv.get() - rx, m.bytes_sent.get() - tx);
    println!(
        "relay fallback_forwarded_rx={rx} fallback_forwarded_tx={tx} qad_connections={}",
        m.qad_connections.get()
    );
    assert!(rx >= CHUNK as u64 && tx >= CHUNK as u64);
    if private_stun() {
        assert_eq!(m.qad_connections.get(), 0);
    } else {
        assert!(m.qad_connections.get() >= 2);
    }
    if let Some(media) = media {
        let m = &media.metrics().server;
        println!(
            "relay media_forwarded_rx={} media_forwarded_tx={}",
            m.bytes_recv.get(),
            m.bytes_sent.get()
        );
        assert!(m.bytes_recv.get() > 0 && m.bytes_sent.get() > 0);
        media.shutdown().await.unwrap();
    }
    if private_stun() {
        wait("relay-only-complete-a").await;
        wait("relay-only-complete-b").await;
    }
    server.shutdown().await.unwrap();
}

fn audio(seq: usize) -> AudioPcmChunk {
    AudioPcmChunk {
        sequence_number: seq as i64,
        pcm_data: vec![seq as u8; 640],
        ..Default::default()
    }
}

// The roles share a kernel and time namespace; this clock is NOT shared
// between physical devices.
fn monotonic_ns() -> i128 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    i128::from(t.tv_sec) * 1_000_000_000 + i128::from(t.tv_nsec)
}

// Sample at a fixed cadence, independent of receipt/loss and stream backpressure.
// Count local send errors as attempts too.
async fn measure(ep: &WanEndpoint) -> EonResult<()> {
    let start = Instant::now();
    let period = Duration::from_millis(20);
    let mut tick = interval_at(start, period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut samples = Vec::new();
    let mut drain = 0;
    let mut steady = [false; 2];
    loop {
        tokio::select! {
            scheduled = tick.tick() => {
                if Path::new("drain").exists() {
                    drain += 1;
                    if drain > 10 { break; }
                    continue;
                }
                let seq = samples.len();
                assert!(seq < 4000);
                let phase = if Path::new("counter-ready").exists() { "fallback" }
                    else if Path::new("cut-start").exists() { "cut" }
                    else if Path::new("direct-a").exists() { "direct" }
                    else if Path::new("open-start").exists() { "opening" }
                    else if Path::new("bootstrap-a").exists() { "bootstrap" }
                    else { "setup" };
                let path = ep.paths().iter().find(|p| p.is_selected()).map(|p| p.remote_addr().clone());
                let frame = audio(seq);
                let now = Instant::now();
                let ns = monotonic_ns();
                // Keep a primary-only PCM echo as a witness on each stable
                // selected path. Never skip a scheduled attempt for a witness.
                let probe = (phase == "direct" && !steady[0]) || (phase == "fallback" && !steady[1]) || phase == "bootstrap";
                let copies = ep.media.is_some() && !probe;
                let sent = if copies { ep.send_redundant_audio(&frame).map(|c| Some((c.primary, c.standby))) }
                           else { ep.send_audio(&frame).map(|_| None) };
                let slot = (scheduled - start).as_nanos() / period.as_nanos();
                println!("send seq={seq} t_us={} phase={phase} mono_ns={ns} slot={slot} path={path:?} queued={sent:?}", (now-start).as_micros());
                samples.push((now, phase, path, false));
                if let Err(e) = sent { println!("audio_send_error seq={seq}: {e}"); }
            }
            frame = ep.recv_audio_observed() => {
                let (frame, route) = frame?;
                let now = Instant::now();
                let seq = usize::try_from(frame.sequence_number).unwrap();
                assert_eq!(frame, audio(seq));
                let (sent, phase, path, seen) = &mut samples[seq];
                assert!(!*seen);
                *seen = true;
                println!("echo seq={seq} rtt_us={}", (now-*sent).as_micros());
                if route == Arrival::Primary && ep.paths().iter().any(|p| p.is_selected() && Some(p.remote_addr()) == path.as_ref()) {
                    if *phase == "bootstrap" && path.as_ref().is_some_and(TransportAddr::is_relay) && !Path::new("bootstrap-audio").exists() {
                        mark("bootstrap-audio");
                    }
                    steady[0] |= *phase == "direct" && path.as_ref().is_some_and(is_direct);
                    steady[1] |= *phase == "fallback" && path.as_ref().is_some_and(TransportAddr::is_relay);
                }
            }
        }
    }
    assert_eq!(steady, [true; 2], "no verified audio echo on both paths");
    Ok(())
}

fn is_direct(addr: &TransportAddr) -> bool {
    addr.is_ip() || addr.is_custom()
}

async fn batch(ep: &WanEndpoint, side: &str, stage: u32) -> EonResult<()> {
    let ctrl = ReliableFrame::Control(EpochInvalidationEvent {
        invalidated_epoch: stage.into(),
        task_id: "path-change".into(),
        ..Default::default()
    });
    let bulk = ReliableFrame::Bulk {
        index: stage,
        bytes: vec![stage as u8; CHUNK],
    };
    let doc = Replica::new(u64::from(stage) * 2 + u64::from(side == "b"))?;
    if side == "a" {
        doc.set("stage", &stage.to_string())?;
        let delta = ReliableFrame::Delta(CrdtDeltaSync {
            crdt_delta_payload: doc.export_all()?,
            ..Default::default()
        });
        let timed = async |lane, frame: &ReliableFrame| {
            let start = Instant::now();
            let result = ep.send(frame).await;
            println!(
                "stream stage={stage} class={lane} receipt_us={} ok={}",
                start.elapsed().as_micros(),
                result.is_ok()
            );
            result
        };
        tokio::try_join!(timed(1, &ctrl), timed(2, &delta), timed(3, &bulk))?;
    } else {
        let (one, two, three) = tokio::try_join!(ep.recv(), ep.recv(), ep.recv())?;
        let frames = [one, two, three];
        assert!(frames.contains(&ctrl) && frames.contains(&bulk));
        let delta = frames
            .iter()
            .find_map(|f| match f {
                ReliableFrame::Delta(d) => Some(d),
                _ => None,
            })
            .unwrap();
        doc.apply(&delta.crdt_delta_payload)?;
        assert_eq!(doc.get("stage"), Some(stage.to_string()));
    }
    Ok(())
}

// Byte deltas for the exact selected address, not total connection statistics.
fn bytes(ep: &WanEndpoint, addr: &TransportAddr, tx: bool) -> Option<u64> {
    let stats = ep
        .paths()
        .iter()
        .find(|p| p.is_selected() && p.remote_addr() == addr)?
        .stats();
    Some(if tx {
        stats.udp_tx.bytes
    } else {
        stats.udp_rx.bytes
    })
}

async fn witness(ep: &WanEndpoint, side: &str, stage: u32, addr: &TransportAddr) -> EonResult<()> {
    let other = if side == "a" { "b" } else { "a" };
    let phase = match stage {
        0 => "bootstrap",
        1 => "direct",
        3 => "fallback",
        _ => unreachable!(),
    };
    let before = timeout(WAIT, async {
        loop {
            if let Some(n) = bytes(ep, addr, side == "a") {
                break n;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {addr:?}; current paths: {:?}", ep.paths()));
    fs::write(format!("{phase}-{side}"), monotonic_ns().to_string()).unwrap();
    wait(&format!("{phase}-{other}")).await;
    if stage == 3 {
        wait("counter-ready").await;
    }
    batch(ep, side, stage).await?;
    let n = bytes(ep, addr, side == "a").unwrap() - before;
    println!(
        "witness side={side} phase={phase} stable_id={} addr={addr:?} wire_bytes={n}",
        ep.conn.stable_id()
    );
    assert!(n >= CHUNK as u64);
    Ok(())
}

async fn peer(side: &str) -> EonResult<()> {
    let (other, host, mapped, remote) = if side == "a" {
        ("b", 1, 2, 3)
    } else {
        ("a", 2, 3, 2)
    };
    let me: SecretKey = fs::read_to_string(format!("keys/{side}.key"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let peer = pin(other);
    let relay: RelayUrl = URL.parse().unwrap();
    let interface_loss = match std::env::var("EON_WAN_CUT").unwrap().as_str() {
        "blackhole" => false,
        "interface-loss" => true,
        _ => panic!("unknown route impairment"),
    };
    let ca = fs::read("keys/ca.der").unwrap();
    let rate = NonZeroU32::new(4 * 1024 * 1024).unwrap();
    // Verify the shipping private-CA, relay-only API, separately from migration.
    let only =
        WanEndpoint::open_with_relay_root(me.clone(), peer, relay.clone(), ca.clone(), rate, WAIT)
            .await?;
    assert_eq!(only.peer_id(), peer);
    batch(&only, side, 4).await?;
    mark(&format!("root-{side}"));
    wait(&format!("root-{other}")).await;
    only.close().await;
    mark(&format!("closed-{side}"));
    wait(&format!("closed-{other}")).await;

    // No peer socket is handed to connect. Discovery reports our *translated* socket;
    // the opposite LAN has no route to this socket's untranslated address.
    let bind = (Ipv4Addr::new(192, 168, host, 2), 41770);
    // Neither timing profile gets a notification of a distant black hole.
    let transport = QuicTransportConfig::builder();
    let transport = match std::env::var("EON_WAN_PATH_PROFILE").unwrap().as_str() {
        "default" => transport,
        "short" => transport
            .default_path_keep_alive_interval(Duration::from_millis(250))
            .default_path_max_idle_timeout(Duration::from_secs(1)),
        _ => panic!("unknown path timing profile"),
    };
    let expected = |i| std::net::SocketAddrV4::new(Ipv4Addr::new(10, 77, 0, i), 41770);
    let config = || -> EonResult<_> {
        Ok(PrivateWanConfig {
            relay: relay.clone(),
            relay_root_der: ca.clone(),
            bind: std::net::SocketAddrV4::new(bind.0, 0),
            stun: std::net::SocketAddrV4::new(IP, 3478),
            stun_username: side.into(),
            stun_password: fs::read_to_string(format!("keys/stun-{side}.psk"))
                .unwrap()
                .trim()
                .as_bytes()
                .to_vec(),
            candidates: CandidateScope::new(Ipv4Addr::new(10, 77, 0, 0), 29, 41728..=41855)?,
        })
    };
    let (mut ep, direct) = if private_stun() {
        let mut reject = config()?;
        reject.bind.set_port(41769);
        reject.stun.set_port(3479);
        reject.stun_username.push_str("_reject");
        assert!(
            matches!(
                WanEndpoint::open_private(me.clone(), peer, reject, rate, WAIT).await,
                Err(EonError::ValidationError(_))
            ),
            "accepted or dialed the publisher's out-of-scope negative control"
        );
        if side == "b" {
            wait("open-second-member").await;
        }
        let ep = WanEndpoint::open_private(me.clone(), peer, config()?, rate, WAIT).await?;
        assert!(
            ep.endpoint.bound_sockets().is_empty(),
            "unrestricted native IP transport!"
        );
        (
            ep,
            TransportAddr::Custom(super::scoped::udp::address(expected(remote))),
        )
    } else if isolated_n0() {
        let pkarr = Ipv4Addr::new(10, 77, 0, 4);
        for provider in [
            N0Provider::Public,
            N0Provider::PrivateMirror {
                relay: pkarr,
                pkarr: IP,
                root_der: ca.clone(),
            },
            N0Provider::PrivateMirror {
                relay: IP,
                pkarr,
                root_der: fs::read("keys/leaf.der").unwrap(),
            },
        ] {
            assert!(matches!(
                WanEndpoint::open_managed_n0(me.clone(), peer, provider, rate, WAIT).await,
                Err(EonError::SecurityViolation(_))
            ));
        }
        // Published by this device, signed by the correct key, but no longer
        // reachable. The HTTPS store never has to forge a DNS record.
        write_stale_record(side, &me);
        let ep = WanEndpoint::open_managed_n0(
            me.clone(),
            peer,
            N0Provider::PrivateMirror {
                relay: IP,
                pkarr,
                root_der: ca.clone(),
            },
            rate,
            WAIT,
        )
        .await?;
        (ep, TransportAddr::Ip(expected(remote).into()))
    } else {
        let builder = relay_builder(transport)
            .bind_addr(bind)
            .unwrap()
            .ca_tls_config(iroh::tls::CaTlsConfig::custom_roots([ca.clone().into()]));
        let ep = WanEndpoint::open_configured(
            me.clone(),
            peer,
            RelayConfig::new(relay.clone(), Some(RelayQuicConfig::new(7842))),
            rate,
            WAIT,
            builder,
        )
        .await?;
        assert!(ep.endpoint.bound_sockets().contains(&bind.into()));
        timeout(WAIT, async {
            while !ep
                .endpoint
                .addr()
                .ip_addrs()
                .any(|a| *a == expected(mapped).into())
            {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("missing QAD mapping");
        (ep, TransportAddr::Ip(expected(remote).into()))
    };
    assert_eq!(ep.peer_id(), peer);
    let primary_id = ep.conn.stable_id();
    if media_enabled() {
        ep.enable_media_standby(MEDIA_URL.parse().unwrap(), ca.clone(), WAIT)
            .await?;
    }
    let app = async {
        let relay = TransportAddr::Relay(ep.relay.clone());
        let relay_path = || {
            ep.paths()
                .iter()
                .find(|p| p.remote_addr() == &relay)
                .map(|p| p.id())
        };
        let mut original = None;
        if !private_stun() {
            witness(&ep, side, 0, &relay).await?;
            wait("bootstrap-audio").await;
            original = Some(relay_path().expect("bootstrap relay path"));
            mark(&format!("bootstrap-proven-{side}"));
            wait("route-open").await;
            ep.endpoint.network_change().await;
        }
        witness(&ep, side, 1, &direct).await?;
        let standby = original.or_else(relay_path).expect("live relay standby");
        let relay_selected = || {
            ep.paths()
                .get(standby)
                .filter(|p| p.remote_addr() == &relay)
                .map(|p| p.is_selected())
        };
        if interface_loss {
            assert_eq!(relay_selected(), Some(false), "no open relay standby");
        }
        if side == "a" {
            sleep(Duration::from_millis(600)).await;
            mark("direct-proven");
        }
        wait("cut").await;
        if interface_loss && side == "a" {
            // Only A lost a local interface. B receives no fixture hint.
            ep.endpoint.network_change().await;
        }
        batch(&ep, side, 2).await?; // receipts may stall; never reconnect or replay
        witness(&ep, side, 3, &relay).await?;
        if interface_loss {
            assert_eq!(
                relay_selected(),
                Some(true),
                "original relay path not reused"
            );
        }
        if side == "a" {
            // Also measure the former long tail after reliable recovery. Keep
            // the cut definition, cadence and <5 ms scoring gate unchanged.
            let watch = if interface_loss && isolated_n0() {
                Duration::from_secs(20)
            } else {
                Duration::from_millis(600)
            };
            sleep(watch).await;
            mark("drain");
        }
        Ok(())
    };
    if side == "a" {
        tokio::try_join!(app, measure(&ep))?;
        mark("finished");
        wait("b-complete").await;
    } else {
        let echo = async {
            loop {
                tokio::select! {
                    chunk = ep.recv_audio_observed() => {
                        let (chunk, via) = chunk?;
                        let received = monotonic_ns();
                        let direct = ep.paths().iter().any(|p| p.is_selected() && is_direct(p.remote_addr()));
                        assert_eq!(chunk, audio(usize::try_from(chunk.sequence_number).unwrap()));
                        if via == Arrival::Primary {
                            ep.send_audio(&chunk)?;
                        } else {
                            ep.send_redundant_audio(&chunk)?;
                        }
                        println!("received seq={} mono_ns={received} via={via:?} direct={direct}", chunk.sequence_number);
                    }
                    _ = sleep(Duration::from_millis(20)) => {
                        if Path::new("finished").exists() { return Ok(()); }
                    }
                }
            }
        };
        tokio::try_join!(app, echo)?;
        mark("b-complete");
    }
    assert_eq!(ep.conn.stable_id(), primary_id);
    if private_stun() {
        // After the scored migration, revoke cached direct and standby consent.
        let only = ep.reconnect(WAIT).await?;
        assert_eq!(only.conn.alpn(), ALPN);
        assert!(only.media.is_none() && only.scoped_root.is_none());
        assert!(only.paths().iter().all(|p| p.remote_addr().is_relay()));
        batch(&only, side, 5).await?;
        mark(&format!("reconnected-{side}"));
        wait(&format!("reconnected-{other}")).await;
        only.close().await;
        mark(&format!("scoped-closed-{side}"));
        wait(&format!("scoped-closed-{other}")).await;
        // Silent Binding, healthy relay: shorter than the full STUN retry
        // schedule. Neither the QUIC timers nor the outer deadline are changed.
        let short = Duration::from_millis(1500);
        let mut silent = config()?;
        silent.stun.set_port(3480);
        let start = Instant::now();
        let fallback = WanEndpoint::open_private(me, peer, silent, rate, short).await?;
        let elapsed = start.elapsed();
        println!(
            "STUN silent side={side} budget_ms=1500 relay_open_us={}",
            elapsed.as_micros()
        );
        assert!(elapsed < short);
        assert_eq!(fallback.peer_id(), peer);
        assert!(fallback.paths().iter().all(|p| p.remote_addr().is_relay()));
        batch(&fallback, side, 6).await?;
        mark(&format!("relay-only-complete-{side}"));
        wait(&format!("relay-only-complete-{other}")).await;
        fallback.close().await;
        return Ok(());
    }
    if isolated_n0() {
        assert!(ep.reconnect(WAIT).await.is_err());
    } else {
        ep.close().await;
    }
    Ok(())
}

fn write_stale_record(side: &str, me: &SecretKey) {
    let stale = iroh::address_lookup::EndpointInfo::new(me.public())
        .with_relay_url(iroh::defaults::prod::default_na_west_relay().url)
        .to_pkarr_signed_packet(me, 30)
        .unwrap()
        .to_relay_payload();
    fs::write(format!("keys/stale-{side}.pkt"), stale).unwrap();
}

#[tokio::test]
#[ignore = "only via scripts/test_wan_nat.sh; requires a disposable isolated topology"]
async fn iroh_and_qos_tests_nat_path_change() -> EonResult<()> {
    let ns = fs::metadata("/proc/self/ns/net").unwrap().ino();
    assert_ne!(
        ns,
        std::env::var("EON_WAN_PARENT_NS")
            .unwrap()
            .parse::<u64>()
            .unwrap()
    );
    assert_eq!(
        Some(ns),
        std::env::var("EON_WAN_NETNS")
            .ok()
            .and_then(|s| s.parse().ok())
    );
    match std::env::var("EON_WAN_ROLE").unwrap().as_str() {
        role @ ("relay" | "field-relay") => {
            relay(role == "field-relay").await;
            Ok(())
        }
        role @ ("field-a" | "field-b") => {
            field::run(role == "field-a").await;
            Ok(())
        }
        role @ ("a" | "b") => peer(role).await,
        _ => panic!("unknown namespace role"),
    }
}
