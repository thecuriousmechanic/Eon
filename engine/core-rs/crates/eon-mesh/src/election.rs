//! Clock-skew-immune spatial election (docs/implementation/05, section 2).
//!
//! Devices announce a claim with a score. The election uses only the local monotonic time at which each
//! claim arrives, never a timestamp the sender supplies, so no sender clock can win or lose the election.
//! The window is 15 ms from the first claim. The incumbent keeps the role unless a challenger beats its score
//! by a margin, which stops two near-equal devices from trading the role back and forth.

use std::collections::BTreeMap;
use std::time::Duration;

use eon_primitives::{EonError, EonResult};
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep_until, timeout_at};

use crate::transport::{Link, PeerNode};

mod probe;
mod ready;
pub use probe::ControlResponder;

pub const WINDOW_NS: u64 = 15_000_000;
/// Challenger must beat the incumbent by this many score points to take over.
pub const HYSTERESIS_MARGIN: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Winner {
    pub node_id: String,
    /// Raw election points; paired sensory claims use thousandths.
    pub score: u32,
}

#[derive(Debug, Default)]
pub struct SpatialElection {
    window_start_ns: Option<u64>,
    claims: BTreeMap<String, u32>,
    incumbent: Option<Winner>,
}

impl SpatialElection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a claim that arrived at local monotonic time `arrived_ns`.
    /// Claims after the window closes are ignored; `poll` must run before they are counted in the next round.
    pub fn on_claim(&mut self, node_id: &str, score: u32, arrived_ns: u64) {
        match self.window_start_ns {
            None => {
                self.window_start_ns = Some(arrived_ns);
                self.claims.insert(node_id.to_string(), score);
            }
            Some(start) if arrived_ns.saturating_sub(start) < WINDOW_NS => {
                let entry = self.claims.entry(node_id.to_string()).or_insert(score);
                *entry = (*entry).max(score);
            }
            Some(_) => {}
        }
    }

    /// Closes the window once `now_ns` is at least 15 ms past its start, then decides the winner.
    pub fn poll(&mut self, now_ns: u64) -> Option<Winner> {
        self.poll_with_margins(now_ns, 0, HYSTERESIS_MARGIN)
    }

    fn poll_with_margins(&mut self, now_ns: u64, tie: u32, margin: u32) -> Option<Winner> {
        let start = self.window_start_ns?;
        if now_ns.saturating_sub(start) < WINDOW_NS {
            return None;
        }
        let highest = self.claims.values().max()?;
        // Compare to the maximum, not pairwise: "within tie points" is not transitive.
        let best = self
            .claims
            .iter()
            .find(|(_, score)| highest - **score <= tie)
            .map(|(id, score)| Winner {
                node_id: id.clone(),
                score: *score,
            });
        self.window_start_ns = None;
        if let Some(current) = &mut self.incumbent
            && let Some(score) = self.claims.get(&current.node_id)
        {
            current.score = *score;
        }
        self.claims.clear();
        let decided = match (&self.incumbent, best) {
            (_, None) => None,
            (Some(current), Some(challenger)) if current.node_id != challenger.node_id => {
                if challenger
                    .score
                    .checked_sub(current.score)
                    .is_some_and(|lead| lead >= margin)
                {
                    Some(challenger)
                } else {
                    Some(current.clone())
                }
            }
            (_, Some(winner)) => Some(winner),
        };
        self.incumbent = decided.clone();
        decided
    }
}

/// Quantized observations, not a peer-supplied score, identity or timestamp.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct SensoryClaim {
    pub snr_db_tenths: i16,
    pub screen_unlocked: bool,
    /// Use `u32::MAX` for no recent local input.
    pub last_hid_activity_ms: u32,
    pub speaker_confidence_permille: u16,
}

impl SensoryClaim {
    /// Score in thousandths: 40% SNR (5..35 dB), 30% recent input, 15% each screen and speaker.
    pub fn score(self) -> u32 {
        let snr = u32::from(self.snr_db_tenths.clamp(50, 350) as u16 - 50);
        (400 * snr + 150) / 300
            + 300 * u32::from(self.last_hid_activity_ms < 2000)
            + 150 * u32::from(self.screen_unlocked)
            + (150 * u32::from(self.speaker_confidence_permille.min(1000)) + 500) / 1000
    }
}

const CLAIM_TAG: [u8; 4] = *b"ESC1";
const WINDOW: Duration = Duration::from_nanos(WINDOW_NS);
const STICKINESS: Duration = Duration::from_secs(15);
const SPEAKER_LEASE: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
struct Datagram {
    tag: [u8; 4],
    turn: u64,
    sensors: SensoryClaim,
    /// Relative to the sender, so neither the key nor a sender clock is needed on the wire.
    was_leader: Option<bool>,
}

impl Datagram {
    fn decode(bytes: &[u8]) -> EonResult<Self> {
        let (msg, rest) = postcard::take_from_bytes::<Self>(bytes).map_err(wire_error)?;
        if msg.tag != CLAIM_TAG || !rest.is_empty() {
            return Err(EonError::ValidationError(
                "invalid spatial claim datagram".into(),
            ));
        }
        Ok(msg)
    }
}

fn wire_error(e: postcard::Error) -> EonError {
    EonError::ValidationError(format!("spatial claim: {e}"))
}

/// One pinned link per pair: election datagrams and optional fixed control-probe
/// streams. It does not dispatch audio, commands or general-purpose RPCs.
pub struct PairedElection {
    link: Link,
    local_id: String,
    peer_id: String,
    last_turn: Option<u64>,
    decision: Option<(Winner, Instant)>,
}

impl PairedElection {
    fn ids(node: &PeerNode) -> EonResult<(String, String)> {
        let local = node.addr().id;
        let peer = node.paired_id().filter(|id| *id != local).ok_or_else(|| {
            EonError::SecurityViolation("spatial election requires a distinct pinned peer".into())
        })?;
        Ok((local.to_string(), peer.to_string()))
    }

    fn new((local_id, peer_id): (String, String), link: Link) -> Self {
        Self {
            link,
            local_id,
            peer_id,
            last_turn: None,
            decision: None,
        }
    }

    pub async fn connect(node: &PeerNode, peer: EndpointAddr) -> EonResult<Self> {
        let ids = Self::ids(node)?;
        Ok(Self::new(ids, node.connect(peer).await?))
    }

    pub async fn accept(node: &PeerNode) -> EonResult<Self> {
        let ids = Self::ids(node)?;
        Ok(Self::new(ids, node.accept().await?))
    }

    /// Local onset. Simultaneous onsets must use the same increasing application turn ID.
    /// `None` means no complete claim set inside the local window; stay silent.
    pub async fn arbitrate(&mut self, turn: u64, local: SensoryClaim) -> EonResult<Option<Winner>> {
        if self.last_turn.is_some_and(|last| turn <= last) {
            return Err(EonError::ValidationError(
                "spatial turn must advance".into(),
            ));
        }
        self.run(turn, local, Instant::now(), None).await
    }

    async fn receive(&self) -> EonResult<(Instant, Datagram)> {
        let bytes = self.link.recv_audio().await?;
        Ok((Instant::now(), Datagram::decode(&bytes)?))
    }

    /// Idle peer: adopt the first fresh remote turn, sample locally at receipt and
    /// return that turn ID so the caller can submit audio through `play_if_leader`.
    pub async fn respond(
        &mut self,
        sample: impl FnOnce() -> SensoryClaim,
    ) -> EonResult<(u64, Option<Winner>)> {
        loop {
            let (start, peer) = self.receive().await.inspect_err(|_| self.decision = None)?;
            if self.last_turn.is_none_or(|last| peer.turn > last) {
                let turn = peer.turn;
                return Ok((turn, self.run(turn, sample(), start, Some(peer)).await?));
            }
        }
    }

    async fn run(
        &mut self,
        turn: u64,
        local: SensoryClaim,
        start: Instant,
        first: Option<Datagram>,
    ) -> EonResult<Option<Winner>> {
        let deadline = start + WINDOW;
        self.last_turn = Some(turn);
        // Invalidate playback *before* any send/await, including on cancellation or loss.
        let was_leader = self
            .decision
            .take()
            .filter(|(_, when)| start.duration_since(*when) < STICKINESS)
            .map(|(winner, _)| winner.node_id == self.local_id);
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let frame = Datagram {
            tag: CLAIM_TAG,
            turn,
            sensors: local,
            was_leader,
        };
        self.link
            .send_audio(&postcard::to_allocvec(&frame).map_err(wire_error)?)?;
        let mut election = SpatialElection::new();
        election.on_claim(&self.local_id, local.score(), 0);
        let (peer, arrived) = match first {
            Some(peer) => (peer, start),
            None => loop {
                let Ok(result) = timeout_at(deadline, self.receive()).await else {
                    return Ok(None);
                };
                let (arrived, peer) = result?;
                if arrived >= deadline {
                    return Ok(None);
                }
                if peer.turn == turn {
                    break (peer, arrived);
                }
            },
        };
        election.on_claim(
            &self.peer_id,
            peer.sensors.score(),
            (arrived - start).as_nanos() as u64,
        );

        // Apply stickiness only on *shared* history. With one-way loss the peers can
        // remember different results; symmetric comparison prevents two local leaders.
        if let (Some(ours), Some(theirs)) = (was_leader, peer.was_leader)
            && ours != theirs
        {
            let (id, incumbent, challenger) = if ours {
                (&self.local_id, local, peer.sensors)
            } else {
                (&self.peer_id, peer.sensors, local)
            };
            if challenger.last_hid_activity_ms >= 1000 {
                election.incumbent = Some(Winner {
                    node_id: id.clone(),
                    score: incumbent.score(),
                });
            }
        }
        sleep_until(deadline).await;
        let winner = election.poll_with_margins(WINDOW_NS, 5, HYSTERESIS_MARGIN * 10);
        self.decision = winner.clone().map(|w| (w, deadline));
        Ok(winner)
    }

    /// Gate each playback submission, rather than caching a leader flag across turns.
    /// Pending, incomplete, follower, stale-turn and expired decisions do not call `play`.
    pub fn play_if_leader(&self, turn: u64, play: impl FnOnce()) -> bool {
        if self.last_turn == Some(turn)
            && self.decision.as_ref().is_some_and(|(w, when)| {
                w.node_id == self.local_id && when.elapsed() < SPEAKER_LEASE
            })
        {
            play();
            return true;
        }
        false
    }
}

#[cfg(test)]
mod spatial_election_tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn highest_score_inside_the_window_wins() {
        let mut e = SpatialElection::new();
        e.on_claim("phone", 40, 0);
        e.on_claim("desktop", 90, 5 * MS);
        e.on_claim("laptop", 60, 9 * MS);
        assert_eq!(e.poll(15 * MS).unwrap().node_id, "desktop");
    }

    #[test]
    fn claims_after_the_window_are_ignored() {
        let mut e = SpatialElection::new();
        e.on_claim("phone", 40, 0);
        e.on_claim("phone", 50, WINDOW_NS - 1);
        e.on_claim("desktop", 90, 15 * MS);
        let winner = e.poll(15 * MS).unwrap();
        assert_eq!(winner.node_id, "phone");
        assert_eq!(winner.score, 50);
    }

    #[test]
    fn no_decision_before_the_window_closes() {
        let mut e = SpatialElection::new();
        e.on_claim("phone", 40, 0);
        assert!(e.poll(14 * MS).is_none());
    }

    #[test]
    fn ties_break_on_node_id_deterministically() {
        let mut e = SpatialElection::new();
        e.on_claim("zeta", 50, 0);
        e.on_claim("alpha", 50, MS);
        assert_eq!(e.poll(15 * MS).unwrap().node_id, "alpha");
    }

    #[test]
    fn incumbent_is_kept_against_a_near_equal_challenger() {
        let mut e = SpatialElection::new();
        e.on_claim("desktop", 80, 0);
        e.poll(15 * MS);
        e.on_claim("laptop", 85, 20 * MS);
        assert_eq!(
            e.poll(35 * MS).unwrap().node_id,
            "desktop",
            "only +5 is below the margin"
        );
    }

    #[test]
    fn a_clear_challenger_takes_over() {
        let mut e = SpatialElection::new();
        e.on_claim("desktop", 80, 0);
        e.poll(15 * MS);
        e.on_claim("laptop", 95, 20 * MS);
        assert_eq!(e.poll(35 * MS).unwrap().node_id, "laptop");
    }

    #[test]
    fn hysteresis_compares_this_rounds_scores_not_the_previous_peak() {
        let mut e = SpatialElection::new();
        e.on_claim("desktop", 100, 0);
        e.poll(WINDOW_NS);
        e.on_claim("desktop", 20, 2 * WINDOW_NS);
        e.on_claim("laptop", 30, 2 * WINDOW_NS);
        assert_eq!(e.poll(3 * WINDOW_NS).unwrap().node_id, "laptop");
    }

    #[test]
    fn sensory_score_clamps_and_weights_each_observation() {
        let mut c = SensoryClaim {
            snr_db_tenths: i16::MIN,
            last_hid_activity_ms: 2000,
            screen_unlocked: false,
            speaker_confidence_permille: 0,
        };
        assert_eq!(c.score(), 0);
        c.snr_db_tenths = 200;
        assert_eq!(c.score(), 200);
        c.snr_db_tenths = i16::MAX;
        assert_eq!(c.score(), 400);
        c.last_hid_activity_ms = 1999;
        assert_eq!(c.score(), 700);
        c.screen_unlocked = true;
        assert_eq!(c.score(), 850);
        c.speaker_confidence_permille = u16::MAX;
        assert_eq!(c.score(), 1000);
    }

    #[test]
    fn near_ties_are_relative_to_the_maximum_regardless_of_arrival_order() {
        let mut claims = [("a", 990), ("b", 995), ("c", 1000)];
        for _ in 0..2 {
            let mut e = SpatialElection::new();
            for (id, score) in claims {
                e.on_claim(id, score, 0);
            }
            assert_eq!(e.poll_with_margins(WINDOW_NS, 5, 100).unwrap().node_id, "b");
            claims.reverse();
        }
    }

    #[test]
    fn an_empty_round_leaves_the_incumbent_in_place() {
        let mut e = SpatialElection::new();
        e.on_claim("desktop", 80, 0);
        e.poll(15 * MS);
        assert!(e.poll(100 * MS).is_none());
        e.on_claim("laptop", 70, 200 * MS);
        assert_eq!(e.poll(215 * MS).unwrap().node_id, "desktop");
    }
}
