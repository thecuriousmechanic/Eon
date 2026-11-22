use super::*;
use iroh_relay::server::{RelayConfig as ServerRelay, Server, ServerConfig};
use std::net::Ipv4Addr;

const WAIT: Duration = Duration::from_secs(6);

// Loopback transport for protocol tests. The public standby API permits no
// HTTP exception; the namespace fixture exercises it with an actual TLS CA.
async fn relay() -> (Server, RelayUrl) {
    let mut config = ServerConfig::default();
    config.relay = Some(ServerRelay::new((Ipv4Addr::LOCALHOST, 0)));
    let server = Server::spawn(config).await.unwrap();
    let url = format!("http://{}", server.http_addr().unwrap())
        .parse()
        .unwrap();
    (server, url)
}

async fn pair(a: &SecretKey, b: &SecretKey, relay: &RelayUrl) -> (WanEndpoint, WanEndpoint) {
    tokio::try_join!(
        WanEndpoint::open(a.clone(), b.public(), relay.clone(), NonZeroU32::MIN, WAIT),
        WanEndpoint::open(b.clone(), a.public(), relay.clone(), NonZeroU32::MIN, WAIT)
    )
    .unwrap()
}

async fn attach(ep: &mut WanEndpoint, url: &RelayUrl) -> EonResult<()> {
    ep.attach_media(
        url.clone(),
        datagram_builder(QuicTransportConfig::builder(), 1),
        WAIT,
    )
    .await
}

#[tokio::test]
async fn iroh_and_qos_tests_primary_capture_rejects_a_copy_before_arming() {
    use crate::wan::continuity::{CapturePlan, receive};

    let (server, url) = relay().await;
    let (tx, rx) = pair(&SecretKey::generate(), &SecretKey::generate(), &url).await;
    let peer = async {
        tx.recv().await.unwrap(); // Primary-only READY.
        let mut packet = vec![COPY_ID_KEY];
        packet.extend_from_slice(&1u64.to_le_bytes());
        AudioPcmChunk {
            sequence_number: 1,
            pcm_data: vec![0; 640],
            ..Default::default()
        }
        .encode_raw(&mut packet);
        tx.conn.send_datagram(packet.into()).unwrap();
    };
    let ((), report) = tokio::join!(
        peer,
        receive(rx, CapturePlan::new(1).unwrap(), || panic!(
            "armed on a copy"
        ))
    );
    assert!(
        report
            .to_string()
            .contains("status=invalid_probe bracketed=false")
    );
    tx.close().await;
    server.shutdown().await.unwrap();
}

#[test]
fn iroh_and_qos_tests_copy_id_cannot_acknowledge_two_different_challenges() {
    let window = Mutex::new(Window::default());
    let encode = |seq: i64| {
        let mut bytes = vec![COPY_ID_KEY];
        bytes.extend_from_slice(&7u64.to_le_bytes());
        AudioPcmChunk {
            sequence_number: seq,
            pcm_data: vec![seq as u8; 16],
            ..Default::default()
        }
        .encode_raw(&mut bytes);
        Bytes::from(bytes)
    };
    assert_eq!(
        decode_copy(encode(1), &window)
            .unwrap()
            .unwrap()
            .sequence_number,
        1
    );
    assert!(decode_copy(encode(2), &window).unwrap().is_none());
}

#[test]
fn iroh_and_qos_tests_media_window_reorder_replay_floor_and_large_jump() {
    let mut w = Window::default();
    for (id, expected) in [
        (0, true),
        (0, false),
        (128, true),
        (1, true),
        (0, false),
        (1, false),
        (129, true),
        (1, false),
        (2, true),
        (u64::MAX, true),
        (u64::MAX - 127, true),
        (129, false),
        (u64::MAX, false),
    ] {
        assert_eq!(w.admit(id), expected);
    }
}

#[tokio::test]
async fn iroh_and_qos_tests_standby_cannot_bind_across_primaries_with_identical_pins() {
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let ((r1, u1), (r2, u2), (m, media)) = tokio::join!(relay(), relay(), relay());
    let ((mut left1, right1), (left2, mut right2)) =
        tokio::join!(pair(&a, &b, &u1), pair(&a, &b, &u2));
    let (l, r) = tokio::join!(attach(&mut left1, &media), attach(&mut right2, &media));
    assert!(l.is_err() && r.is_err());
    assert!(
        [l, r]
            .iter()
            .any(|v| matches!(v, Err(EonError::SecurityViolation(_))))
    );
    assert!(left1.media.is_none() && right2.media.is_none());
    let pcm = AudioPcmChunk {
        pcm_data: vec![3; 640],
        ..Default::default()
    };
    left1.send_audio(&pcm).unwrap();
    assert_eq!(
        timeout(WAIT, right1.recv_audio()).await.unwrap().unwrap(),
        pcm
    );
    tokio::join!(left1.close(), right1.close(), left2.close(), right2.close());
    let (x, y, z) = tokio::join!(r1.shutdown(), r2.shutdown(), m.shutdown());
    x.unwrap();
    y.unwrap();
    z.unwrap();
}

#[tokio::test]
async fn iroh_and_qos_tests_asymmetric_attach_keeps_primary_pcm_and_loss_never_offloads() {
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let ((server, url), (m, media)) = tokio::join!(relay(), relay());
    let (mut left, mut right) = pair(&a, &b, &url).await;
    tokio::try_join!(attach(&mut left, &media), attach(&mut right, &media)).unwrap();
    let remote = right.media.take().unwrap(); // peer did not install its binder result
    let pcm = AudioPcmChunk {
        pcm_data: vec![9; 640],
        ..Default::default()
    };
    left.send_redundant_audio(&pcm).unwrap();
    assert_eq!(
        timeout(WAIT, right.recv_audio()).await.unwrap().unwrap(),
        pcm
    );
    let packet = timeout(WAIT, remote.conn.read_datagram())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(wire::decode_audio(packet.clone()).unwrap(), pcm);

    // Directly exercise enqueue after the outer liveness check: the primary
    // may close between that check and send_datagram.
    left.conn.close(0u8.into(), b"primary lost during send");
    assert!(
        left.media
            .as_ref()
            .unwrap()
            .enqueue(&left.conn, packet)
            .is_err()
    );
    assert!(
        timeout(Duration::from_millis(100), remote.conn.read_datagram())
            .await
            .is_err()
    );
    tokio::join!(left.close(), right.close(), remote.close());
    let (x, y) = tokio::join!(server.shutdown(), m.shutdown());
    x.unwrap();
    y.unwrap();
}

#[tokio::test]
async fn iroh_and_qos_tests_primary_close_cancels_rendezvous_and_stalled_binder() {
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    for stall_binder in [false, true] {
        let ((server, url), (m, media)) = tokio::join!(relay(), relay());
        let (mut l, r) = pair(&a, &b, &url).await;
        let remote = bind(
            datagram_builder(QuicTransportConfig::builder(), 1),
            b.clone(),
            RelayConfig::new(media.clone(), None),
            MEDIA_ALPN,
        )
        .await
        .unwrap();
        let primary = l.conn.clone();
        let (done, handled) = tokio::sync::oneshot::channel();
        let terminate = async {
            let _held = if stall_binder {
                Some(
                    timeout(WAIT, rendezvous(&remote, a.public(), &media, MEDIA_ALPN))
                        .await
                        .unwrap()
                        .unwrap(),
                )
            } else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                None
            };
            // In the second case keep the authenticated media connection open
            // without ever replying to the binder. Only primary loss can wake setup.
            tokio::time::sleep(Duration::from_millis(30)).await;
            primary.close(0u8.into(), b"cancel attachment");
            handled.await.unwrap();
        };
        let (err, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(
                async {
                    let result = attach(&mut l, &media).await;
                    done.send(()).unwrap();
                    result
                },
                terminate
            )
        })
        .await
        .expect("attachment waited for its own timeout");
        assert!(err.is_err() && l.media.is_none());
        tokio::join!(l.close(), r.close(), remote.close());
        let (x, y) = tokio::join!(server.shutdown(), m.shutdown());
        x.unwrap();
        y.unwrap();
    }
}

#[tokio::test]
async fn iroh_and_qos_tests_primary_revoked_before_drains_and_reconnect_has_one_deadline() {
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let ((s, url), (m, media)) = tokio::join!(relay(), relay());
    let (mut l, mut r) = pair(&a, &b, &url).await;
    tokio::try_join!(attach(&mut l, &media), attach(&mut r, &media)).unwrap();
    let primary = l.conn.clone();
    let r_primary = r.conn.clone();
    let r_media = r.media.as_ref().unwrap().conn.clone();
    // Leave the media transport without a reachable peer for graceful drain.
    m.shutdown().await.unwrap();
    let mut reconnect = Box::pin(l.reconnect(Duration::from_millis(10)));
    tokio::select! {
        biased;
        _ = &mut reconnect => panic!("reconnected without consent"),
        _ = std::future::ready(()) => {}
    }
    assert!(primary.close_reason().is_some());
    assert!(
        timeout(Duration::from_millis(100), reconnect)
            .await
            .expect("drain exceeded reconnect deadline")
            .is_err()
    );
    let mut close = Box::pin(r.close());
    let drained = tokio::select! {
        biased;
        _ = &mut close => true,
        _ = std::future::ready(()) => false,
    };
    assert!(r_primary.close_reason().is_some() && r_media.close_reason().is_some());
    if !drained {
        close.await;
    }
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn iroh_and_qos_tests_standby_requires_reciprocal_consent_for_each_primary() {
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let ((server, url), (m, media)) = tokio::join!(relay(), relay());
    let (mut left, mut right) = pair(&a, &b, &url).await;
    let one_side = left
        .attach_media(
            media.clone(),
            datagram_builder(QuicTransportConfig::builder(), 1),
            Duration::from_millis(200),
        )
        .await;
    assert!(one_side.is_err() && left.media.is_none());
    tokio::try_join!(attach(&mut left, &media), attach(&mut right, &media)).unwrap();
    // A healthy, bound media transport must not keep a closed primary alive.
    let lane = left.media.as_ref().unwrap();
    assert!(lane.conn.close_reason().is_none());
    left.conn.close(0u8.into(), b"test end of logical session");
    let pcm = AudioPcmChunk {
        pcm_data: vec![1; 640],
        ..Default::default()
    };
    assert!(left.send_redundant_audio(&pcm).is_err());
    assert!(left.recv_audio().await.is_err());
    let (left, right) = tokio::try_join!(left.reconnect(WAIT), right.reconnect(WAIT)).unwrap();
    assert!(left.send_redundant_audio(&pcm).is_err() && right.send_redundant_audio(&pcm).is_err());
    left.send_audio(&pcm).unwrap();
    assert_eq!(
        timeout(WAIT, right.recv_audio()).await.unwrap().unwrap(),
        pcm
    );
    tokio::join!(left.close(), right.close());
    let (x, y) = tokio::join!(server.shutdown(), m.shutdown());
    x.unwrap();
    y.unwrap();
}

#[tokio::test]
async fn iroh_and_qos_tests_bound_media_is_explicit_deduped_and_independent_of_streams() {
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let ((server, url), (m, media)) = tokio::join!(relay(), relay());
    let (mut left, mut right) = pair(&a, &b, &url).await;
    let pcm = AudioPcmChunk {
        pcm_data: vec![7; 640],
        ..Default::default()
    };
    assert!(left.send_redundant_audio(&pcm).is_err());
    for url in [
        "https://example.invalid",
        "https://203.0.113.1",
        "http://127.0.0.1",
    ] {
        assert!(matches!(
            left.enable_media_standby(url.parse().unwrap(), vec![1], WAIT)
                .await,
            Err(EonError::ValidationError(_))
        ));
    }
    tokio::try_join!(attach(&mut left, &media), attach(&mut right, &media)).unwrap();
    let recv = || timeout(WAIT, right.recv_audio_observed());
    left.send_audio(&pcm).unwrap();
    assert_eq!(
        recv().await.unwrap().unwrap(),
        (pcm.clone(), Arrival::Primary)
    );
    assert_eq!(
        left.media
            .as_ref()
            .unwrap()
            .conn
            .paths()
            .iter()
            .map(|p| p.stats().frame_tx.datagram)
            .sum::<u64>(),
        0
    );
    assert!(
        left.send_redundant_audio(&AudioPcmChunk {
            pcm_data: vec![0; 1144],
            ..Default::default()
        })
        .is_err()
    );

    let bulk = ReliableFrame::Bulk {
        index: 1,
        bytes: vec![42; 65_536],
    };
    let mut pending = Box::pin(left.send(&bulk));
    let (mut receipt, mut stalled) = tokio::select! {
        result = &mut pending => panic!("unexpected stream receipt: {result:?}"),
        streams = right.conn.accept_bi() => streams.unwrap(),
    };
    // The peer hasn't read the stream or receipted it. Send identical PCM
    // twice (including its app sequence); each emission must be played once.
    for _ in 0..2 {
        assert_eq!(
            left.send_redundant_audio(&pcm).unwrap(),
            AudioCopies {
                primary: true,
                standby: true
            }
        );
        let (got, via) = recv().await.unwrap().unwrap();
        assert_eq!(got, pcm);
        assert_ne!(via, Arrival::Primary);
        assert!(
            timeout(Duration::from_millis(30), recv()).await.is_err(),
            "duplicate playout"
        );
    }
    let wire = stalled.read_to_end(70_000).await.unwrap();
    assert_eq!(wire.len(), 65_536 + 4 + wire::HEADER);
    receipt.write_all(&[RECEIVED]).await.unwrap();
    receipt.finish().unwrap();
    pending.await.unwrap();

    // Loss of only the copy transport is not a new session and does not
    // interrupt the primary. Conversely, a closed primary authorizes nothing.
    left.media
        .as_ref()
        .unwrap()
        .conn
        .close(0u8.into(), b"test media loss");
    assert_eq!(
        left.send_redundant_audio(&pcm).unwrap(),
        AudioCopies {
            primary: true,
            standby: false
        }
    );
    assert_eq!(
        recv().await.unwrap().unwrap(),
        (pcm.clone(), Arrival::PrimaryCopy)
    );
    left.conn.close(0u8.into(), b"test primary loss");
    // Close between dequeue and provenance check: do not discard the
    // primary packet or allow an already-dequeued standby copy to substitute.
    let packet = Bytes::from_static(b"dequeued");
    let (primary, standby) = after_dequeue(&left.conn, packet.clone(), false).unwrap();
    assert!(!standby && primary == packet);
    assert!(after_dequeue(&left.conn, packet, true).is_err());
    assert!(left.send_redundant_audio(&pcm).is_err());
    assert!(left.recv_audio().await.is_err());
    tokio::join!(left.close(), right.close());
    let (x, y) = tokio::join!(server.shutdown(), m.shutdown());
    x.unwrap();
    y.unwrap();
}
