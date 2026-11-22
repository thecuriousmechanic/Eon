//! One-round state exchange between two replicas over the transport (docs/implementation/05, section 3).
//!
//! The initiator offers its version vector and its full state. The responder applies that state and answers with the
//! updates the initiator lacks, and the initiator applies the answer. Both sides converge after one round trip. Repeating
//! a round is harmless because imports are idempotent. Paired nodes instead use the incremental protocol in
//! [`crate::paired`]; the unpinned full-state protocol is retained for existing low-level callers.

use eon_primitives::{EonError, EonResult};
use iroh::EndpointAddr;

use crate::qos::TrafficClass;
use crate::sync::Replica;
use crate::transport::PeerNode;

fn exchange_err(reason: &str) -> EonError {
    EonError::OperationFailed {
        operation: "mesh.exchange".into(),
        reason: reason.into(),
    }
}

/// The initiator's message: its version vector, length-prefixed, followed by its full state.
pub fn offer(replica: &Replica) -> EonResult<Vec<u8>> {
    let version = replica.version();
    let mut out = (version.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&version);
    out.extend_from_slice(&replica.export_all()?);
    Ok(out)
}

/// The responder's side: applies the initiator's state and returns the updates the initiator lacks.
pub fn answer(replica: &Replica, offer: &[u8]) -> EonResult<Vec<u8>> {
    let (version, update) = split(offer)?;
    replica.apply(update)?;
    replica.export_since(version)
}

fn split(bytes: &[u8]) -> EonResult<(&[u8], &[u8])> {
    let header: [u8; 4] = bytes
        .get(..4)
        .ok_or_else(|| exchange_err("offer is shorter than its header"))?
        .try_into()
        .map_err(|_| exchange_err("offer header is malformed"))?;
    let len = u32::from_le_bytes(header) as usize;
    bytes[4..].split_at_checked(len).ok_or_else(|| exchange_err("offer version is truncated"))
}

/// Offers `replica` to `peer` and applies the answer. Afterwards `replica` holds everything `peer` held.
pub async fn exchange(client: &PeerNode, peer: EndpointAddr, replica: &Replica) -> EonResult<()> {
    if client.paired_id().is_some() {
        return crate::paired::sync(client, peer, replica).await;
    }
    let reply = client.request_with_class(peer, TrafficClass::Delta, &offer(replica)?).await?;
    replica.apply(&reply)
}

/// Picks up to `fanout` distinct indices out of `count` for one gossip round. The same `seed` always picks the same
/// indices, so a schedule can be replayed.
pub fn pick_neighbours(count: usize, fanout: usize, seed: u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..count).collect();
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for i in (1..count).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        order.swap(i, (state % (i as u64 + 1)) as usize);
    }
    order.truncate(fanout.min(count));
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_are_distinct_in_range_and_capped_at_the_neighbour_count() {
        let picks = pick_neighbours(10, 3, 7);
        assert_eq!(picks.len(), 3);
        assert!(picks.iter().all(|&i| i < 10));
        let mut unique = picks.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 3);
        assert_eq!(pick_neighbours(2, 5, 1).len(), 2);
    }

    #[test]
    fn the_same_seed_picks_the_same_neighbours() {
        assert_eq!(pick_neighbours(8, 3, 42), pick_neighbours(8, 3, 42));
    }

    #[test]
    fn a_truncated_offer_is_refused() {
        let replica = Replica::new(1).unwrap();
        assert!(answer(&replica, &[9, 0, 0, 0, 1]).is_err());
        assert!(answer(&replica, &[1]).is_err());
    }

    #[test]
    fn offer_and_answer_converge_two_replicas_in_memory() {
        let a = Replica::new(1).unwrap();
        let b = Replica::new(2).unwrap();
        a.set("from_a", "1").unwrap();
        b.set("from_b", "2").unwrap();
        let reply = answer(&b, &offer(&a).unwrap()).unwrap();
        a.apply(&reply).unwrap();
        assert_eq!(a.get("from_b").as_deref(), Some("2"));
        assert_eq!(b.get("from_a").as_deref(), Some("1"));
    }
}
