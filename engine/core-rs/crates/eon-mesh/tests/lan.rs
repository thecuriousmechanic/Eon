use std::{net::Ipv4Addr, time::Duration};

use eon_mesh::{EonError, election::SensoryClaim, lan::open_election};
use iroh::SecretKey;

const WAIT: Duration = Duration::from_secs(10);

#[tokio::test]
#[ignore = "opt in to real mDNS multicast on an attached private IPv4 LAN"]
async fn mdns_to_pinned_tls_to_claims_over_lan_interface() {
    let a = SecretKey::generate();
    let b = SecretKey::generate();
    let (a_id, b_id) = (a.public(), b.public());
    // Neither caller has the other's socket, and no unsigned address is passed to the dialer.
    let ((a, mut left), (b, mut right)) = tokio::try_join!(
        open_election(a, b_id, None, 1, WAIT),
        open_election(b, a_id, None, 1, WAIT),
    )
    .unwrap();
    assert_eq!((a.paired_id(), b.paired_id()), (Some(b_id), Some(a_id)));
    let quiet = SensoryClaim {
        snr_db_tenths: 50,
        screen_unlocked: false,
        last_hid_activity_ms: u32::MAX,
        speaker_confidence_permille: 0,
    };
    let active = SensoryClaim {
        screen_unlocked: true,
        ..quiet
    };
    let (leader, (turn, other)) = tokio::time::timeout(WAIT, async {
        tokio::try_join!(left.arbitrate(1, active), right.respond(|| quiet))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(turn, 1);
    assert_eq!(leader, other);
    assert_eq!(leader.unwrap().node_id, a_id.to_string());
    assert!(left.play_if_leader(turn, || {}));
    assert!(!right.play_if_leader(turn, || panic!("follower must stay silent")));
    tokio::join!(a.close(), b.close());
}

#[tokio::test]
async fn mdns_does_not_fall_back_from_an_ineligible_interface() {
    let result = open_election(
        SecretKey::generate(),
        SecretKey::generate().public(),
        Some(Ipv4Addr::LOCALHOST),
        1,
        WAIT,
    )
    .await;
    assert!(
        matches!(result, Err(EonError::OperationFailed { reason, .. }) if reason.contains("interface"))
    );
}

#[tokio::test]
async fn mdns_never_bootstraps_a_self_pair() {
    let key = SecretKey::generate();
    let id = key.public();
    assert!(matches!(
        open_election(key, id, None, 1, WAIT).await,
        Err(EonError::SecurityViolation(_))
    ));
}
