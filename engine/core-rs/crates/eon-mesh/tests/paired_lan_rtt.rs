//! The LAN gate runs one copy per node with an explicit interface and ephemeral
//! pinned keys. The separately selected loopback calibration skips discovery.

use eon_mesh::election::{PairedElection, SensoryClaim};
use eon_mesh::lan::open_election;
use eon_mesh::transport::PeerNode;
use iroh::{EndpointAddr, PublicKey, SecretKey, TransportAddr};
use std::net::Ipv4Addr;
use std::time::Duration;

const SETUP_DEADLINE: Duration = Duration::from_secs(15);
const ELECTION_DEADLINE: Duration = Duration::from_secs(5);
const TRIALS: usize = 100;
const MAX_RTT: Duration = Duration::from_millis(10);

#[tokio::test]
#[ignore = "requires two isolated or explicitly authorized private LAN nodes"]
async fn election_and_rpc_on_separate_lan_nodes() {
    let required = |key| std::env::var(key).expect(key);
    let initiator = match required("EON_TEST_LAN_ROLE").as_str() {
        "initiator" => true,
        "responder" => false,
        _ => panic!("expected initiator or responder"),
    };
    let ip = required("EON_TEST_LAN_IP").parse().unwrap();
    // Fixture files contain hex-encoded keys; never print them or accept a network URL.
    let secret: SecretKey = std::fs::read_to_string(required("EON_TEST_LAN_KEY_FILE"))
        .unwrap()
        .trim()
        .parse()
        .expect("invalid local fixture key");
    let peer: PublicKey = required("EON_TEST_LAN_PEER")
        .parse()
        .expect("invalid pinned peer");
    let (node, link) = open_election(secret, peer, Some(ip), 1, SETUP_DEADLINE)
        .await
        .expect("pinned LAN discovery and QUIC setup (15 s; requires mDNS egress)");
    established_trial(node, link, peer, initiator).await;
}

// Separate component measurement. The LAN test above never calls this or retries on loopback.
#[tokio::test]
#[ignore = "explicit loopback calibration only; not LAN validation"]
async fn election_and_rpc_on_explicit_loopback() {
    let (a, b) = (SecretKey::generate(), SecretKey::generate());
    let (a_id, b_id) = (a.public(), b.public());
    let a = PeerNode::start_paired(a, b_id).await.unwrap();
    let b = PeerNode::start_paired(b, a_id).await.unwrap();
    let port = b.addr().ip_addrs().find(|s| s.is_ipv4()).unwrap().port();
    let addr = EndpointAddr::from_parts(
        b_id,
        [TransportAddr::Ip((Ipv4Addr::LOCALHOST, port).into())],
    );
    let (left, right) = tokio::time::timeout(SETUP_DEADLINE, async {
        tokio::try_join!(
            PairedElection::connect(&a, addr),
            PairedElection::accept(&b)
        )
    })
    .await
    .expect("loopback setup deadline")
    .unwrap();
    tokio::join!(
        established_trial(a, left, b_id, true),
        established_trial(b, right, a_id, false)
    );
}

async fn established_trial(
    node: PeerNode,
    mut link: PairedElection,
    peer: PublicKey,
    initiator: bool,
) {
    let claim = SensoryClaim {
        snr_db_tenths: if initiator { 350 } else { 50 },
        screen_unlocked: false,
        last_hid_activity_ms: u32::MAX,
        speaker_confidence_permille: 0,
    };
    let responder = (!initiator).then(|| link.control_responder());
    let elect_by = tokio::time::Instant::now() + ELECTION_DEADLINE;
    let elect = async {
        let (turn, result) = tokio::time::timeout_at(elect_by, async {
            if initiator {
                (1, link.arbitrate(1, claim).await.unwrap())
            } else {
                link.respond(|| claim).await.unwrap()
            }
        })
        .await
        .expect("paired election deadline");
        assert_eq!(turn, 1);
        let expected = if initiator { node.addr().id } else { peer };
        assert_eq!(
            result.expect("no matching peer claim inside the 15 ms election window").node_id,
            expected.to_string()
        );
        assert_eq!(link.play_if_leader(turn, || {}), initiator);
    };
    let slowest = if let Some(responder) = responder {
        // Our claim can arrive later than the initiator's. Its first real RPC
        // mustn't queue behind the remainder of our local election window.
        tokio::join!(elect, async {
            responder
                .answer_initial_control_probe(elect_by)
                .await
                .unwrap();
            for _ in 1..TRIALS {
                responder.answer_control_probe().await.unwrap();
            }
        });
        link.wait_for_control_peer_close().await.unwrap();
        None
    } else {
        elect.await;
        let mut rtts = Vec::with_capacity(TRIALS);
        for _ in 0..TRIALS {
            rtts.push(link.measure_control_rtt().await.unwrap());
        }
        let first = rtts[0];
        let (worst, _) = rtts.iter().enumerate().max_by_key(|(_, rtt)| *rtt).unwrap();
        let late = rtts.iter().filter(|&&rtt| rtt >= MAX_RTT).count();
        rtts.sort_unstable();
        eprintln!(
            "paired control RPC (n={TRIALS}): p50={:?} p95={:?} max={:?} (RPC #{}) first={first:?} missed={late}",
            rtts[49],
            rtts[94],
            rtts[99],
            worst + 1
        );
        Some(rtts[TRIALS - 1])
    };
    // Close even if a measurement missed its target. Otherwise the responder
    // can report a peer timeout that obscures a completed but slow exchange.
    node.close().await;
    if let Some(max) = slowest {
        assert!(
            max < MAX_RTT,
            "not all {TRIALS} RPCs finished in <10 ms: {max:?}"
        );
    }
}
