use super::PairedElection;
use eon_primitives::{EonError, EonResult};

const READY: &[u8] = b"ELAN1:ready";

fn unavailable(_: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: "mesh.lan".into(),
        reason: "election readiness stream unavailable".into(),
    }
}

impl PairedElection {
    /// Both pinned LAN peers send after leaving discovery, under open_election's
    /// setup deadline. A uni stream is neither a sensory datagram nor one of
    /// the measured bidirectional control requests.
    pub(crate) async fn confirm_lan_ready(&self) -> EonResult<()> {
        let conn = &self.link.connection;
        let mut send = conn.open_uni().await.map_err(unavailable)?;
        send.write_all(READY).await.map_err(unavailable)?;
        send.finish().map_err(unavailable)?;

        let mut recv = conn.accept_uni().await.map_err(unavailable)?;
        if recv.read_to_end(READY.len()).await.map_err(unavailable)? != READY {
            return Err(EonError::ValidationError(
                "invalid LAN readiness marker".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::election::{SensoryClaim, WINDOW_NS};
    use crate::transport::PeerNode;
    use iroh::{EndpointAddr, SecretKey, TransportAddr};
    use std::{net::Ipv4Addr, time::Duration};
    use tokio::time::timeout;

    async fn pair() -> (PeerNode, PeerNode, PairedElection, PairedElection) {
        let (a, b) = (SecretKey::generate(), SecretKey::generate());
        let aid = a.public();
        let left = PeerNode::start_paired(a, b.public()).await.unwrap();
        let right = PeerNode::start_paired(b, aid).await.unwrap();
        let port = right
            .addr()
            .ip_addrs()
            .find(|a| a.is_ipv4())
            .unwrap()
            .port();
        let addr = EndpointAddr::from_parts(
            right.addr().id,
            [TransportAddr::Ip((Ipv4Addr::LOCALHOST, port).into())],
        );
        let (a, b) = tokio::try_join!(
            PairedElection::connect(&left, addr),
            PairedElection::accept(&right)
        )
        .unwrap();
        (left, right, a, b)
    }

    #[tokio::test]
    async fn established_transport_is_not_peer_readiness() {
        let (left, _right, mut a, mut b) = pair().await;
        {
            let mut waiting = std::pin::pin!(a.confirm_lan_ready());
            // QUIC is connected but B hasn't left setup. Even three election
            // windows later A cannot begin its first turn.
            assert!(
                timeout(Duration::from_nanos(3 * WINDOW_NS), &mut waiting)
                    .await
                    .is_err()
            );
            timeout(Duration::from_secs(2), async {
                tokio::try_join!(waiting, b.confirm_lan_ready())
            })
            .await
            .unwrap()
            .unwrap();
        }
        assert!(!a.play_if_leader(1, || panic!("readiness cannot authorize playback")));
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
        let (winner, (turn, other)) = timeout(Duration::from_secs(2), async {
            tokio::try_join!(a.arbitrate(1, active), b.respond(|| quiet))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(winner, other);
        assert_eq!(winner.unwrap().node_id, left.addr().id.to_string());
        assert!(a.play_if_leader(turn, || {}));
        assert!(!b.play_if_leader(turn, || panic!("follower")));
    }

    #[tokio::test]
    async fn readiness_requires_the_exact_marker_and_fin() {
        let (_left, _right, a, b) = pair().await;
        let mut send = b.link.connection.open_uni().await.unwrap();
        send.write_all(READY).await.unwrap();
        let mut waiting = std::pin::pin!(a.confirm_lan_ready());
        assert!(
            timeout(Duration::from_millis(50), &mut waiting)
                .await
                .is_err()
        );
        send.write_all(b"!").await.unwrap();
        send.finish().unwrap();
        assert!(
            timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap()
                .is_err()
        );
    }
}
