use super::{Server, endpoint, relay};
use eon_mesh::{
    EonResult,
    wan::{
        AudioPcmChunk, ReliableFrame, WanEndpoint,
        continuity::{CapturePlan, receive, transmit, transmit_observed},
    },
};
use iroh::SecretKey;
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use tokio::{sync::oneshot, time::sleep};

async fn pair() -> EonResult<(Server, WanEndpoint, WanEndpoint)> {
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let (aid, bid) = (a.public(), b.public());
    let (server, url) = relay(&[&a, &b]).await;
    let (tx, rx) = tokio::try_join!(endpoint(a, bid, &url), endpoint(b, aid, &url))?;
    Ok((server, tx, rx))
}

// Use the existing pin-only loopback relay fixture, never construct a fake
// grant or select a different constructor in the executable.
#[tokio::test]
async fn iroh_and_qos_tests_continuity_pair() -> EonResult<()> {
    continuity_pair(false).await?;
    continuity_pair(true).await
}

async fn continuity_pair(timing: bool) -> EonResult<()> {
    let (server, tx, rx) = pair().await?;
    let before = server.metrics().server.bytes_sent.get();
    let mut plan = CapturePlan::new(2).unwrap();
    if timing {
        plan = plan.with_decode_receipts();
    }
    let marks = || {
        let (tx, rx) = mpsc::channel();
        tokio::spawn(async move {
            for _ in 0..2 {
                sleep(Duration::from_millis(250)).await;
                tx.send(Instant::now()).unwrap();
            }
        });
        rx
    };
    let (sender, captured) = tokio::join!(transmit_observed(tx, plan), receive(rx, plan, marks));
    let sent = sender.result().unwrap();
    assert!((3..=100).contains(&sent));
    let text = captured.to_string();
    assert!(captured.bracketed(), "{text}");
    assert!(text.contains("phase=during") && text.contains(&format!("queued={sent} ")));
    if timing {
        let report = sender.to_string();
        let binding = report
            .lines()
            .next()
            .unwrap()
            .strip_prefix(
                "eon_continuity_timing_v3 clock=sender_monotonic mode=primary capture_id=",
            )
            .unwrap();
        assert_eq!(binding.len(), 64);
        assert!(binding.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(text.starts_with(&format!(
            "eon_continuity_v2 clock=receiver_monotonic mode=primary capture_id={binding}\n"
        )));
        assert!(
            report.contains("status=complete timing=observed"),
            "{report}"
        );
        assert_eq!(
            report.lines().filter(|l| l.starts_with("seq=")).count(),
            sent as usize
        );
        assert!(text.contains("timing=see_sender"));
    } else {
        assert!(text.contains("timing=unavailable"));
    }
    assert!(server.metrics().server.bytes_sent.get() - before >= 640 * 3);
    server.shutdown().await.unwrap();
    Ok(())
}

fn chunk(seq: i64) -> AudioPcmChunk {
    AudioPcmChunk {
        sequence_number: seq,
        pcm_data: vec![0; 640],
        ..Default::default()
    }
}

const READY: &[u8] = b"eon/continuity/2/challenge/ready";
const START: &[u8] = b"eon/continuity/2/challenge/start";
const STARTED: &[u8] = b"eon/continuity/2/challenge/started";

fn timed_chunk(seq: i64, challenge: u8) -> AudioPcmChunk {
    let mut pcm = chunk(seq);
    pcm.pcm_data[..2].copy_from_slice(b"T2");
    pcm.pcm_data[2..18].fill(challenge);
    pcm
}

fn control(tag: &[u8], index: u32) -> ReliableFrame {
    ReliableFrame::Bulk {
        index,
        bytes: tag.to_vec(),
    }
}

#[tokio::test]
async fn iroh_and_qos_tests_continuity_timed_sender_refuses_legacy_ready() -> EonResult<()> {
    let (server, tx, rx) = pair().await?;
    let plan = CapturePlan::new(1).unwrap();
    let (sent, capture) = tokio::join!(
        transmit_observed(tx, plan.with_decode_receipts()),
        receive(rx, plan, || panic!("probed a legacy receiver"))
    );
    assert_eq!(sent.result(), Err("invalid_ready_or_plan"));
    assert!(
        sent.to_string()
            .contains("timing=unavailable limit=50 queued=0")
    );
    assert!(!capture.bracketed());
    server.shutdown().await.unwrap();
    Ok(())
}

#[tokio::test]
async fn iroh_and_qos_tests_continuity_timed_receiver_requires_matching_start() -> EonResult<()> {
    let (server, tx, rx) = pair().await?;
    let plan = CapturePlan::new(1).unwrap().with_decode_receipts();
    let peer = async {
        assert_eq!(tx.recv().await?, control(READY, 50));
        tx.send_audio(&timed_chunk(1, 1))?; // transport ACK is not timing consent
        let _ = tx
            .send(&control(
                b"eon/continuity/2/challenge/start-bound-copies",
                50,
            ))
            .await;
        assert!(tx.recv_audio().await.is_err(), "echoed without consent");
        EonResult::Ok(())
    };
    let (res, capture) = tokio::join!(peer, receive(rx, plan, || panic!("armed before START")));
    res?;
    let report = capture.to_string();
    assert!(report.contains("status=invalid_start_or_plan") && report.contains("unique=0"));
    assert!(report.contains("timing=unavailable"));
    tx.close().await;
    server.shutdown().await.unwrap();
    Ok(())
}

#[tokio::test]
async fn iroh_and_qos_tests_continuity_decode_without_echo_has_no_timing_bound() -> EonResult<()> {
    let (server, tx, rx) = pair().await?;
    let plan = CapturePlan::new(1).unwrap().with_decode_receipts();
    let peer = async {
        rx.send(&control(READY, 50)).await?;
        assert_eq!(rx.recv().await?, control(START, 50));
        rx.send(&control(STARTED, 50)).await?;
        let first = rx.recv_audio().await?;
        let second = rx.recv_audio().await?;
        for pcm in [&first, &second] {
            assert_eq!(pcm.pcm_data.len(), 640);
            assert!(pcm.pcm_data.starts_with(b"T2") && pcm.pcm_data[18..].iter().all(|&b| b == 0));
        }
        assert_ne!(first.sequence_number, second.sequence_number);
        assert!(first.pcm_data[2..18] != second.pcm_data[2..18]);
        // Neither a predictable receipt nor another valid sequence's challenge
        // acknowledges the second probe.
        for wrong in [b"ok".to_vec(), first.pcm_data[2..18].to_vec()] {
            rx.send_audio(&AudioPcmChunk {
                sequence_number: second.sequence_number,
                pcm_data: wrong,
                ..Default::default()
            })?;
        }
        let ReliableFrame::Bulk { index, bytes } = rx.recv().await? else {
            panic!("missing finish");
        };
        assert_eq!(bytes.as_slice(), b"eon/continuity/2/queued");
        rx.send(&control(b"eon/continuity/2/unique", 2)).await?;
        rx.close().await;
        EonResult::Ok(index)
    };
    let (queued, sender) = tokio::time::timeout(super::WAIT, async {
        tokio::join!(peer, transmit_observed(tx, plan))
    })
    .await
    .expect("timed sender stalled");
    let queued = queued?;
    assert_eq!(sender.result(), Ok(queued));
    let report = sender.to_string();
    assert!(report.contains(&format!(
        "skipped={} acknowledged=0 unacknowledged={queued} peer_missing={} decoded_without_receipt=2",
        50 - queued, queued - 2
    )));
    assert!(report.contains("max_decode_receipt_rtt_upper_us=unavailable"));
    assert!(
        !report.contains("invalid_returns=0\n"),
        "bad returns not counted"
    );
    server.shutdown().await.unwrap();
    Ok(())
}

#[tokio::test]
async fn iroh_and_qos_tests_continuity_copy_plan_requires_a_bound_lane() -> EonResult<()> {
    let (server, tx, rx) = pair().await?;
    let plan = CapturePlan::new(2).unwrap().with_redundant_media();
    let (sent, captured) = tokio::join!(
        transmit(tx, plan),
        receive(rx, plan, || panic!("armed without a standby"))
    );
    assert_eq!(sent, Err("missing_bound_standby"));
    assert!(
        captured
            .to_string()
            .contains("status=missing_bound_standby bracketed=false")
    );
    server.shutdown().await.unwrap();
    Ok(())
}

#[tokio::test]
async fn iroh_and_qos_tests_continuity_peer_cannot_upgrade_a_primary_only_sender() -> EonResult<()>
{
    let (server, tx, rx) = pair().await?;
    let plan = CapturePlan::new(2).unwrap();
    let peer = async {
        // The sender must decode and reject READY, but its close can
        // overtake the transport receipt for that rejected frame.
        let _ = rx
            .send(&ReliableFrame::Bulk {
                index: 100,
                bytes: b"eon/continuity/1/ready-bound-copies".to_vec(),
            })
            .await;
        assert!(rx.recv_audio().await.is_err(), "unexpected probe");
    };
    let (sent, ()) = tokio::join!(transmit(tx, plan), peer);
    assert_eq!(sent, Err("invalid_ready_or_plan"));
    rx.close().await;
    server.shutdown().await.unwrap();
    Ok(())
}

#[tokio::test]
async fn iroh_and_qos_tests_continuity_missing_and_late_tail() -> EonResult<()> {
    let (server, tx, rx) = pair().await?;
    let plan = CapturePlan::new(1).unwrap().with_decode_receipts();
    let (first, started) = oneshot::channel();
    let peer = async {
        assert_eq!(tx.recv().await?, control(READY, 50));
        tx.send(&control(START, 50)).await?;
        assert_eq!(tx.recv().await?, control(STARTED, 50));
        tx.send_audio(&timed_chunk(2, 42))?;
        started.await.unwrap();
        let echo = tx.recv_audio().await?;
        assert_eq!(echo.sequence_number, 2);
        assert!(echo.pcm_data == [42; 16]);
        // Inject a duplicate, reordered tail after FINISH, and an absent seq=3.
        tx.send_audio(&timed_chunk(2, 99))?;
        sleep(plan.span()).await;
        tx.send(&control(b"eon/continuity/2/queued", 3)).await?;
        sleep(Duration::from_millis(50)).await;
        tx.send_audio(&timed_chunk(1, 17))?;
        assert_eq!(
            tx.recv_audio().await?.sequence_number,
            1,
            "duplicate echoed"
        );
        assert_eq!(tx.recv().await?, control(b"eon/continuity/2/unique", 2));
        let _ = tx.recv_audio().await; // let the receiver finish the drain receipt
        EonResult::Ok(())
    };
    let (res, captured) = tokio::join!(
        peer,
        receive(rx, plan, || {
            first.send(()).unwrap();
            mpsc::channel().1 // no operator marks
        })
    );
    res?;
    let text = captured.to_string();
    assert!(!captured.bracketed());
    assert!(
        text.contains(
            "status=complete bracketed=false limit=50 queued=3 unique=2 missing=1 duplicates=1"
        ),
        "{text}"
    );
    assert!(text.contains("seq=2 ") && text.contains("seq=1 "));
    tx.close().await;
    server.shutdown().await.unwrap();
    Ok(())
}

#[tokio::test]
async fn iroh_and_qos_tests_continuity_delayed_start_rejects_early_finish() -> EonResult<()> {
    let (server, tx, rx) = pair().await?;
    let plan = CapturePlan::new(2).unwrap().with_decode_receipts();
    let peer = async {
        assert_eq!(tx.recv().await?, control(READY, 100));
        sleep(plan.span()).await;
        tx.send(&control(START, 100)).await?;
        assert_eq!(tx.recv().await?, control(STARTED, 100));
        let _ = tx.send(&control(b"eon/continuity/2/queued", 0)).await;
        assert!(
            tx.recv().await.is_err(),
            "drained without a full post-START span"
        );
        EonResult::Ok(())
    };
    let (res, capture) = tokio::time::timeout(super::WAIT, async {
        tokio::join!(peer, receive(rx, plan, || panic!("armed without a probe")))
    })
    .await
    .expect("delayed START handshake stalled");
    res?;
    assert!(
        capture
            .to_string()
            .contains("status=early_finish bracketed=false")
    );
    tx.close().await;
    server.shutdown().await.unwrap();
    Ok(())
}

#[tokio::test]
async fn iroh_and_qos_tests_continuity_early_finish_keeps_partial_receipts() -> EonResult<()> {
    let (server, tx, rx) = pair().await?;
    let plan = CapturePlan::new(60).unwrap();
    let (first, started) = oneshot::channel();
    let peer = async {
        tx.recv().await?;
        tx.send_audio(&chunk(1))?;
        started.await.unwrap();
        // A consistent count, but far too soon for the agreed capture interval.
        let _ = tx
            .send(&ReliableFrame::Bulk {
                index: 1,
                bytes: b"eon/continuity/1/queued".to_vec(),
            })
            .await;
        assert!(tx.recv().await.is_err()); // no DRAINED confirmation
        tx.close().await;
        EonResult::Ok(())
    };
    let (res, capture) = tokio::time::timeout(super::WAIT, async {
        tokio::join!(
            peer,
            receive(rx, plan, || {
                first.send(()).unwrap();
                mpsc::channel().1
            })
        )
    })
    .await
    .expect("reject instead of waiting out the agreed span");
    res?;
    let text = capture.to_string();
    assert!(text.contains("status=early_finish bracketed=false"));
    assert!(text.contains("queued=unknown unique=1 missing=unknown"));
    server.shutdown().await.unwrap();
    Ok(())
}

#[tokio::test]
async fn iroh_and_qos_tests_continuity_reconnect_or_malformed_is_not_a_capture() -> EonResult<()> {
    for fault in ["reconnect", "wrong_pcm", "short_total"] {
        let (server, tx, rx) = pair().await?;
        let plan = CapturePlan::new(1).unwrap();
        let (first, started) = oneshot::channel();
        let peer = async {
            tx.recv().await?;
            tx.send_audio(&chunk(1))?;
            started.await.unwrap();
            match fault {
                "reconnect" => assert!(tx.reconnect(Duration::from_millis(250)).await.is_err()),
                "wrong_pcm" => {
                    let mut malformed = chunk(2);
                    malformed.pcm_data[0] = 42;
                    tx.send_audio(&malformed)?;
                    let _ = tx.recv_audio().await; // receiver rejects and closes
                    tx.close().await;
                }
                _ => {
                    let _ = tx
                        .send(&ReliableFrame::Bulk {
                            index: 0,
                            bytes: b"eon/continuity/1/queued".to_vec(),
                        })
                        .await;
                    let _ = tx.recv_audio().await;
                    tx.close().await;
                }
            }
            EonResult::Ok(())
        };
        let (res, captured) = tokio::join!(
            peer,
            receive(rx, plan, || {
                first.send(()).unwrap();
                mpsc::channel().1
            })
        );
        res?;
        let text = captured.to_string();
        assert!(!captured.bracketed() && !text.contains("status=complete"));
        assert!(
            text.contains("queued=unknown unique=1 missing=unknown"),
            "{text}"
        );
        match fault {
            "short_total" => assert!(text.contains("status=inconsistent_finish")),
            "wrong_pcm" => assert!(text.contains("status=invalid_probe")),
            _ => (),
        }
        server.shutdown().await.unwrap();
    }
    Ok(())
}
