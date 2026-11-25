//! Established, mutually pinned QUIC connections. Dial only the local test peer's loopback port.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use eon_mesh::EonError;
use eon_mesh::election::{PairedElection, SensoryClaim, WINDOW_NS};
use eon_mesh::transport::PeerNode;
use iroh::{EndpointAddr, SecretKey, TransportAddr};
use tokio::time::timeout;

const BOUND: Duration = Duration::from_secs(5);
const BASE: SensoryClaim = SensoryClaim {
    snr_db_tenths: 200,
    screen_unlocked: true,
    last_hid_activity_ms: 1500,
    speaker_confidence_permille: 800,
};
const BETTER: SensoryClaim = SensoryClaim {
    snr_db_tenths: 250,
    ..BASE
};

fn loopback(node: &PeerNode) -> EndpointAddr {
    let addr = node.addr();
    let port = addr.ip_addrs().find(|a| a.is_ipv4()).unwrap().port();
    EndpointAddr::from_parts(
        addr.id,
        [TransportAddr::Ip((Ipv4Addr::LOCALHOST, port).into())],
    )
}

async fn pair() -> (PeerNode, PeerNode, PairedElection, PairedElection) {
    let a = SecretKey::from_bytes(&[1; 32]);
    let b = SecretKey::from_bytes(&[2; 32]);
    let a_key = a.public();
    let left = PeerNode::start_paired(a, b.public()).await.unwrap();
    let right = PeerNode::start_paired(b, a_key).await.unwrap();
    let (a, b) = timeout(BOUND, async {
        tokio::try_join!(
            PairedElection::connect(&left, loopback(&right)),
            PairedElection::accept(&right),
        )
    })
    .await
    .unwrap()
    .unwrap();
    (left, right, a, b)
}

async fn elect(
    a: &mut PairedElection,
    b: &mut PairedElection,
    turn: u64,
    left: SensoryClaim,
    right: SensoryClaim,
    expected: &str,
) {
    let start = Instant::now();
    let (wa, wb) = tokio::try_join!(a.arbitrate(turn, left), b.arbitrate(turn, right)).unwrap();
    assert!(start.elapsed() >= Duration::from_nanos(WINDOW_NS));
    assert_eq!(wa, wb);
    assert_eq!(wa.unwrap().node_id, expected);
}

#[tokio::test]
async fn control_rtt_uses_an_existing_paired_link_and_does_not_elect_a_speaker() {
    let (_left, _right, a, b) = pair().await;
    for _ in 0..3 {
        let (rtt, ()) =
            tokio::try_join!(a.measure_control_rtt(), b.answer_control_probe()).unwrap();
        assert!(rtt > Duration::ZERO && rtt < BOUND);
    }
    assert!(!a.play_if_leader(1, || panic!("a probe cannot elect a leader")));
    assert!(!b.play_if_leader(1, || panic!("a probe cannot elect a leader")));
    // B remains connected but idle: none of these operations should hang.
    let (request, answer, close) = timeout(BOUND, async {
        tokio::join!(
            a.measure_control_rtt(),
            a.answer_control_probe(),
            a.wait_for_control_peer_close()
        )
    })
    .await
    .expect("all control deadlines must fire");
    for (err, expected) in [
        (request.unwrap_err(), "control probe request timed out"),
        (answer.unwrap_err(), "control probe responder timed out"),
        (close.unwrap_err(), "control peer close timed out"),
    ] {
        assert!(
            matches!(err, EonError::OperationFailed { operation, reason }
            if operation == "mesh.paired_probe" && reason == expected)
        );
    }
    drop(b);
    assert!(
        timeout(BOUND, a.measure_control_rtt())
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn first_probe_waits_for_bounded_startup_before_the_request() {
    let (_left, _right, a, b) = pair().await;
    let responder = b.control_responder();
    let accept_by = tokio::time::Instant::now() + BOUND;
    tokio::try_join!(responder.answer_initial_control_probe(accept_by), async {
        // No stream or request exists yet. Exceed the ordinary two-second
        // budget without spending any of the initiator's measured RPC time.
        tokio::time::sleep(Duration::from_millis(2100)).await;
        a.measure_control_rtt().await
    })
    .unwrap();
    let err = responder
        .answer_initial_control_probe(tokio::time::Instant::now())
        .await
        .unwrap_err();
    assert!(
        matches!(err, EonError::OperationFailed { operation, reason }
        if operation == "mesh.paired_probe" && reason == "timed out waiting for first control stream")
    );
}

#[tokio::test]
async fn control_reply_does_not_wait_for_the_local_election_to_finish() {
    let (_left, _right, a, mut b) = pair().await;
    let responder = b.control_responder();
    {
        let mut election = std::pin::pin!(b.arbitrate(1, BASE));
        // Send the local claim, but don't supply a peer claim or drive this
        // election to completion until the independent stream reply arrives.
        assert!(
            futures_lite::future::poll_once(election.as_mut())
                .await
                .is_none()
        );
        tokio::try_join!(a.measure_control_rtt(), responder.answer_control_probe()).unwrap();
        assert!(election.await.unwrap().is_none());
    }
    assert!(!b.play_if_leader(1, || panic!("a probe is not a claim")));
}

#[tokio::test]
async fn spatial_election_tests_simultaneous_claims_tie_stick_then_handoff_with_one_playback_sink()
{
    let (left, right, mut a, mut b) = pair().await;
    let (left_id, right_id) = (left.addr().id.to_string(), right.addr().id.to_string());
    let (incumbent, challenger) = if left_id < right_id {
        (&left_id, &right_id)
    } else {
        (&right_id, &left_id)
    };
    assert!(!a.play_if_leader(1, || panic!("unelected")));
    elect(&mut a, &mut b, 1, BASE, BASE, incumbent).await;
    let mut played = Vec::new();
    a.play_if_leader(1, || played.push(&left_id));
    b.play_if_leader(1, || played.push(&right_id));
    assert_eq!(played, [incumbent]);

    let by_role = |challenge| {
        if incumbent == &left_id {
            (BASE, challenge)
        } else {
            (challenge, BASE)
        }
    };
    let (x, y) = by_role(BETTER);
    elect(&mut a, &mut b, 2, x, y, incumbent).await;
    let (x, y) = by_role(SensoryClaim {
        last_hid_activity_ms: 999,
        ..BETTER
    });
    elect(&mut a, &mut b, 3, x, y, challenger).await;
    a.play_if_leader(3, || played.push(&left_id));
    b.play_if_leader(3, || played.push(&right_id));
    assert_eq!(played, [incumbent, challenger]);
    assert!(!a.play_if_leader(2, || panic!("old turn")));
    assert!(!b.play_if_leader(2, || panic!("old turn")));
}

#[tokio::test]
async fn spatial_election_tests_idle_peer_answers_and_missing_or_stale_claims_do_not_authorize_playback()
 {
    let (left, right, mut a, mut b) = pair().await;
    let (wa, (turn, wb)) = timeout(BOUND, async {
        tokio::try_join!(a.arbitrate(10, BETTER), b.respond(|| BASE))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(turn, 10);
    assert_eq!(wa, wb);
    assert_eq!(wa.unwrap().node_id, left.addr().id.to_string());
    assert!(a.play_if_leader(10, || {}));
    assert!(!b.play_if_leader(turn, || panic!("idle follower")));

    // The local window closes without a response. Even the old winner must now be silent.
    assert!(a.arbitrate(11, BASE).await.unwrap().is_none());
    assert!(!a.play_if_leader(10, || panic!("superseded")));
    assert!(!a.play_if_leader(11, || panic!("incomplete")));
    assert!(a.arbitrate(11, BETTER).await.is_err());

    // Deliver the reply to turn 11 *after* A's deadline. B now has an incumbent
    // that A doesn't remember. A must ignore the stale reply in turn 12; neither
    // side should apply a unilateral hysteresis bonus.
    let fresh = SensoryClaim {
        last_hid_activity_ms: 0,
        ..BETTER
    };
    let (turn, wb) = timeout(BOUND, b.respond(|| fresh)).await.unwrap().unwrap();
    assert_eq!(turn, 11);
    assert_eq!(wb.unwrap().node_id, right.addr().id.to_string());
    elect(
        &mut a,
        &mut b,
        12,
        BETTER,
        BASE,
        &left.addr().id.to_string(),
    )
    .await;
    assert!(a.play_if_leader(12, || {}));
    assert!(!b.play_if_leader(12, || panic!("unilateral incumbent")));
}

#[tokio::test]
async fn spatial_election_tests_unpinned_endpoints_are_rejected_before_accepting_claims() {
    let node = PeerNode::start().await.unwrap();
    assert!(matches!(
        timeout(BOUND, PairedElection::accept(&node)).await.unwrap(),
        Err(EonError::SecurityViolation(_))
    ));
}
