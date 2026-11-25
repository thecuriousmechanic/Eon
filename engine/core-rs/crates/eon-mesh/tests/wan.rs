//! Real relay transit on loopback; not a carrier roaming or STUN test.

#[path = "wan/continuity.rs"]
mod continuity;

use std::{
    net::{Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    sync::Arc,
    time::Duration,
};

use eon_mesh::{
    EonResult,
    blob::Manifest,
    sync::{ContextEntry, Replica},
    wan::{AudioPcmChunk, CrdtDeltaSync, EpochInvalidationEvent, ReliableFrame, WanEndpoint},
};
use iroh::{EndpointId, RelayUrl, SecretKey};
use iroh_relay::server::{Access, AccessControl, ClientRequest, RelayConfig, Server, ServerConfig};
use prost::Message;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct Allowed(Vec<EndpointId>);

impl AccessControl for Allowed {
    async fn on_connect(&self, req: &ClientRequest) -> Access {
        if self.0.contains(&req.endpoint_id()) {
            Access::Allow
        } else {
            Access::Deny { reason: None }
        }
    }
}

async fn relay(keys: &[&SecretKey]) -> (Server, RelayUrl) {
    relay_at((Ipv4Addr::LOCALHOST, 0).into(), keys).await
}

async fn relay_at(addr: SocketAddr, keys: &[&SecretKey]) -> (Server, RelayUrl) {
    let mut relay = RelayConfig::new(addr);
    relay.access = Arc::new(Allowed(keys.iter().map(|k| k.public()).collect()));
    let mut config = ServerConfig::default();
    config.relay = Some(relay); // no QUIC discovery server, metrics listener or ACME
    let server = Server::spawn(config).await.unwrap();
    let url = format!("http://{}", server.http_addr().unwrap())
        .parse()
        .unwrap();
    (server, url)
}

async fn endpoint(secret: SecretKey, pin: EndpointId, relay: &RelayUrl) -> EonResult<WanEndpoint> {
    WanEndpoint::open(
        secret,
        pin,
        relay.clone(),
        NonZeroU32::new(1024 * 1024).unwrap(),
        WAIT,
    )
    .await
}

async fn exchange_four_lanes(
    left: &WanEndpoint,
    right: &WanEndpoint,
    relay: &Server,
) -> EonResult<()> {
    timeout(WAIT, async {
        let stats = &relay.metrics().server;
        // Start after the relay and QUIC handshakes; observe data forwarding,
        // not just successful relay registration or a path hint.
        let before = (stats.bytes_recv.get(), stats.bytes_sent.get());
        let audio = AudioPcmChunk {
            sequence_number: 1,
            timestamp_ms: 20,
            pcm_data: (0..320i16).flat_map(i16::to_le_bytes).collect(),
            priority_class: 0,
        };
        let epoch = ReliableFrame::Control(EpochInvalidationEvent {
            invalidated_epoch: 7,
            task_id: "local-turn".into(),
            reason: "barge-in".into(),
        });
        let (source, sink) = (Replica::new(1)?, Replica::new(2)?);
        source.set("model", "v2")?;
        source.put_context(
            "private",
            &ContextEntry::Clipboard {
                text: "not exported".into(),
            },
        )?;
        let delta = ReliableFrame::Delta(CrdtDeltaSync {
            ledger_id: "projection".into(),
            lamport_clock: 1,
            crdt_delta_payload: source.export_since(&sink.version())?,
        });
        let chunk_size = 64 * 1024;
        let mut data = vec![0; 2 * chunk_size];
        blake3::Hasher::new().finalize_xof().fill(&mut data);
        let manifest = Manifest::for_bytes(&data, chunk_size)?;
        let bulk_len = data.len() as u64;
        let second = ReliableFrame::Bulk {
            index: 1,
            bytes: data.split_off(chunk_size),
        };
        let first = ReliableFrame::Bulk {
            index: 0,
            bytes: data,
        };

        assert!(
            left.send_audio(&AudioPcmChunk {
                pcm_data: vec![0; 1150],
                ..audio.clone()
            })
            .is_err()
        );
        assert!(
            left.send(&ReliableFrame::Bulk {
                index: 0,
                bytes: vec![0; 65537]
            })
            .await
            .is_err()
        );
        let send = async {
            left.send_audio(&audio)?;
            tokio::try_join!(
                left.send(&first),
                left.send(&second),
                left.send(&delta),
                left.send(&epoch)
            )?;
            let echoed = left.recv_audio().await?;
            assert_eq!(echoed, audio);
            Ok(())
        };
        let receive = async {
            let (pcm, a, b, c, d) = tokio::try_join!(
                right.recv_audio(),
                right.recv(),
                right.recv(),
                right.recv(),
                right.recv()
            )?;
            assert_eq!(pcm, audio);
            let mut seen = [0; 4];
            for frame in [a, b, c, d] {
                match frame {
                    ReliableFrame::Control(_) => {
                        assert_eq!(frame, epoch);
                        seen[0] += 1;
                    }
                    ReliableFrame::Delta(change) => {
                        assert_eq!(change.ledger_id, "projection");
                        assert_eq!(change.lamport_clock, 1);
                        sink.apply(&change.crdt_delta_payload)?;
                        assert_eq!(sink.get("model").as_deref(), Some("v2"));
                        assert!(sink.working_context()?.is_empty());
                        seen[1] += 1;
                    }
                    ReliableFrame::Bulk { index, bytes } => {
                        manifest.verify_chunk(index as usize, &bytes)?;
                        seen[2 + index as usize] += 1;
                    }
                }
            }
            assert_eq!(seen, [1, 1, 1, 1]);
            right.send_audio(&pcm)
        };
        tokio::try_join!(send, receive)?;
        assert!(stats.bytes_recv.get() - before.0 >= bulk_len);
        assert!(stats.bytes_sent.get() - before.1 >= bulk_len);
        Ok(())
    })
    .await
    .expect("bounded relay roundtrip")
}

#[tokio::test]
async fn iroh_and_qos_tests_pinned_relay_carries_all_four_lanes_on_one_connection() -> EonResult<()>
{
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let (aid, bid) = (a.public(), b.public());
    let (relay, url) = relay(&[&a, &b]).await;
    let (left, right) = tokio::try_join!(endpoint(a, bid, &url), endpoint(b, aid, &url))?;
    exchange_four_lanes(&left, &right, &relay).await?;
    tokio::join!(left.close(), right.close());
    relay.shutdown().await.unwrap();
    Ok(())
}

fn voice_frame(seq: u8, pcm_bytes: usize) -> AudioPcmChunk {
    AudioPcmChunk {
        sequence_number: i64::from(seq),
        pcm_data: vec![seq; pcm_bytes],
        ..Default::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn iroh_and_qos_tests_voice_burst_replaces_unsent_audio() -> EonResult<()> {
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let (aid, bid) = (a.public(), b.public());
    let (relay, url) = relay(&[&a, &b]).await;
    let (left, right) = tokio::try_join!(endpoint(a, bid, &url), endpoint(b, aid, &url))?;

    for (tx, rx) in [(&left, &right), (&right, &left)] {
        let sent = || {
            tx.paths()
                .iter()
                .map(|p| p.stats().frame_tx.datagram)
                .sum::<u64>()
        };
        let before = sent();
        // No await in this burst: the same-thread QUIC driver cannot drain
        // the sender between frames.
        for seq in 1..=8 {
            tx.send_audio(&voice_frame(seq, 640))?;
        }
        let latest = timeout(WAIT, rx.recv_audio()).await.unwrap()?;
        assert_eq!(latest.sequence_number, 8);
        assert_eq!(sent() - before, 1, "stale datagrams left the sender");

        // A maximum-size legal chunk must still fit, even with PCM already queued.
        tx.send_audio(&voice_frame(9, 640))?;
        let full = voice_frame(128, 1144);
        assert_eq!(full.encoded_len(), 1150);
        tx.send_audio(&full)?;
        assert!(timeout(WAIT, rx.recv_audio()).await.unwrap()? == full);
        assert_eq!(sent() - before, 2);
    }
    tokio::join!(left.close(), right.close());
    relay.shutdown().await.unwrap();
    Ok(())
}

#[tokio::test]
async fn iroh_and_qos_tests_slow_reader_drops_old_audio_after_arrival() -> EonResult<()> {
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let (aid, bid) = (a.public(), b.public());
    let (relay, url) = relay(&[&a, &b]).await;
    let (left, right) = tokio::try_join!(endpoint(a, bid, &url), endpoint(b, aid, &url))?;

    for (tx, rx) in [(&left, &right), (&right, &left)] {
        let received = || {
            rx.paths()
                .iter()
                .map(|p| p.stats().frame_rx.datagram)
                .sum::<u64>()
        };
        for latest in [voice_frame(2, 640), voice_frame(128, 1144)] {
            // Prove both frames reached QUIC before reading either. Otherwise
            // sender eviction or network loss could make an unbounded reader pass.
            for frame in [&voice_frame(1, 640), &latest] {
                let before = received();
                tx.send_audio(frame)?;
                timeout(WAIT, async {
                    while received() == before {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("datagram did not arrive");
            }
            let frame = timeout(WAIT, rx.recv_audio()).await.unwrap()?;
            assert_eq!(frame.sequence_number, latest.sequence_number);
            assert!(frame == latest);
        }
    }
    tokio::join!(left.close(), right.close());
    relay.shutdown().await.unwrap();
    Ok(())
}

#[tokio::test]
async fn iroh_and_qos_tests_relay_membership_does_not_authorize_an_application_peer()
-> EonResult<()> {
    let (desk, other, edge) = (
        SecretKey::generate(),
        SecretKey::generate(),
        SecretKey::generate(),
    );
    let (server, url) = relay(&[&desk, &other, &edge]).await;
    let desk_id = desk.public();
    let accepting = endpoint(desk, edge.public(), &url);
    tokio::pin!(accepting);
    // A configured relay member knows the real destination key, but has no
    // application access. Refusing it must not abort the intended rendezvous.
    tokio::select! {
        _ = &mut accepting => panic!("desk should still be waiting for its pinned peer"),
        result = endpoint(other, desk_id, &url) => assert!(result.is_err()),
    }
    let (edge, desk) = tokio::try_join!(endpoint(edge, desk_id, &url), accepting)?;
    tokio::join!(desk.close(), edge.close());
    server.shutdown().await.unwrap();
    Ok(())
}

#[tokio::test]
async fn iroh_and_qos_tests_reconnect_after_relay_restart_preserves_peer_and_qos() -> EonResult<()>
{
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let (aid, bid) = (a.public(), b.public());
    let (server, url) = relay(&[&a, &b]).await;
    let addr = server.http_addr().unwrap();
    let (left, right) = tokio::try_join!(
        WanEndpoint::open(a.clone(), bid, url.clone(), NonZeroU32::MIN, WAIT),
        endpoint(b.clone(), aid, &url)
    )?;
    assert_eq!((left.peer_id(), right.peer_id()), (bid, aid));

    // At 1 B/s, admitting this chunk reserves more bulk time than the entire test.
    let chunk = ReliableFrame::Bulk {
        index: 0,
        bytes: vec![13; 4096],
    };
    let transfer = async |tx: &WanEndpoint, rx: &WanEndpoint, frame: &ReliableFrame| {
        tokio::try_join!(tx.send(frame), rx.recv())
    };
    let before = server.metrics().server.bytes_sent.get();
    let ((), received) = timeout(WAIT, transfer(&left, &right, &chunk))
        .await
        .unwrap()?;
    assert_eq!(received, chunk);
    assert!(server.metrics().server.bytes_sent.get() - before >= 4096);

    server.shutdown().await.unwrap();
    let lost = ReliableFrame::Control(EpochInvalidationEvent {
        invalidated_epoch: 99,
        ..Default::default()
    });
    let short = Duration::from_millis(250);
    assert!(!matches!(
        timeout(short, transfer(&left, &right, &lost)).await,
        Ok(Ok(_))
    ));

    let recover = async { tokio::try_join!(left.reconnect(WAIT), right.reconnect(WAIT)) };
    tokio::pin!(recover);
    assert!(timeout(short, &mut recover).await.is_err());
    let (server, restarted_url) = relay_at(addr, &[&a, &b]).await;
    assert_eq!(restarted_url, url);
    let (left, right) = recover.await?;
    assert_eq!((left.peer_id(), right.peer_id()), (bid, aid));
    // The undelivered epoch must not be replayed on the replacement connection.
    let fresh = ReliableFrame::Control(EpochInvalidationEvent {
        invalidated_epoch: 100,
        ..Default::default()
    });
    let ((), received) = timeout(WAIT, transfer(&left, &right, &fresh))
        .await
        .unwrap()?;
    assert_eq!(received, fresh);
    // The restarted relay has fresh counters. Verify payload transit, not just
    // two new registrations.
    exchange_four_lanes(&right, &left, &server).await?;
    tokio::select! {
        sent = left.send(&chunk) => panic!("reserved bulk send did not wait: {sent:?}"),
        recv = right.recv() => panic!("unexpected traffic during bulk reservation: {recv:?}"),
        _ = tokio::time::sleep(short) => {}
    }
    tokio::join!(left.close(), right.close());
    server.shutdown().await.unwrap();
    Ok(())
}

#[tokio::test]
async fn iroh_and_qos_tests_reconnect_requires_fresh_consent_from_the_original_peer()
-> EonResult<()> {
    let (a, b, outsider) = (
        SecretKey::generate(),
        SecretKey::generate(),
        SecretKey::generate(),
    );
    let (aid, bid) = (a.public(), b.public());
    let (server, url) = relay(&[&a, &b, &outsider]).await;
    let (left, right) = tokio::try_join!(endpoint(a, bid, &url), endpoint(b.clone(), aid, &url))?;
    right.close().await;

    let recover = left.reconnect(WAIT);
    tokio::pin!(recover);
    tokio::select! {
        _ = &mut recover => panic!("reconnected without the pinned peer"),
        other = endpoint(outsider, aid, &url) => assert!(other.is_err()),
    }
    // The original peer can explicitly rejoin even if it lost its QUIC state.
    let (left, right) = tokio::try_join!(recover, endpoint(b, aid, &url))?;
    assert_eq!((left.peer_id(), right.peer_id()), (bid, aid));
    // This time the other peer is still online, but has not agreed to rejoin.
    assert!(
        timeout(
            Duration::from_secs(2),
            left.reconnect(Duration::from_millis(250))
        )
        .await
        .expect("reconnect must respect its deadline")
        .is_err()
    );
    right.close().await;
    server.shutdown().await.unwrap();
    Ok(())
}
