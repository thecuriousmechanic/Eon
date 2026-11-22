//! Four traffic classes sharing one peer connection (docs/implementation/05, section 2.2).
//!
//! Audio travels as unreliable QUIC datagrams so a late frame is dropped instead of stalling the stream behind it.
//! The other classes use streams. Higher stream priority is transmitted first, so control traffic preempts CRDT deltas,
//! and deltas preempt bulk transfers.

use std::time::Duration;

/// Traffic class of a message. Each class has one transport and one scheduling rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficClass {
    /// Microphone and playback frames. Datagrams only, never retransmitted.
    Audio,
    /// Epoch commits, tokens, and handshakes. Highest stream priority.
    Control,
    /// CRDT deltas and keyframes.
    Delta,
    /// Model weights and build blobs. Lowest priority, so it yields to everything else.
    Bulk,
}

impl TrafficClass {
    /// Stream scheduler priority, or `None` for `Audio`, which never uses a stream.
    pub fn stream_priority(self) -> Option<i32> {
        match self {
            TrafficClass::Audio => None,
            TrafficClass::Control => Some(100),
            TrafficClass::Delta => Some(50),
            TrafficClass::Bulk => Some(0),
        }
    }
}

/// Paces bulk transfer so it cannot starve interactive traffic. While audio, control, or delta traffic was seen in the last
/// 200 ms, bulk may use only a quarter of the link. Otherwise it may use all of it. Times are offsets from a common start
/// so the pacing is deterministic under test.
pub struct BulkPacer {
    link_bytes_per_sec: f64,
    last_interactive: Option<Duration>,
    next_bulk_at: Duration,
}

const BUSY_WINDOW: Duration = Duration::from_millis(200);
const BUSY_SHARE: f64 = 0.25;

impl BulkPacer {
    pub fn new(link_bytes_per_sec: f64) -> Self {
        Self {
            link_bytes_per_sec,
            last_interactive: None,
            next_bulk_at: Duration::ZERO,
        }
    }

    /// Records traffic of `class` observed at `at`. Bulk traffic does not count as interactive.
    pub fn observe(&mut self, class: TrafficClass, at: Duration) {
        if class != TrafficClass::Bulk {
            self.last_interactive = Some(at);
        }
    }

    pub(crate) fn next_bulk_at(&self) -> Duration {
        self.next_bulk_at
    }

    /// Admits a bulk chunk of `bytes` requested at `at`. Returns the time it may start, and reserves link time for it.
    pub fn admit(&mut self, at: Duration, bytes: usize) -> Duration {
        let start = at.max(self.next_bulk_at);
        let busy = self
            .last_interactive
            .is_some_and(|seen| start.saturating_sub(seen) < BUSY_WINDOW);
        let rate = if busy {
            self.link_bytes_per_sec * BUSY_SHARE
        } else {
            self.link_bytes_per_sec
        };
        self.next_bulk_at = start + Duration::from_secs_f64(bytes as f64 / rate);
        start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_never_takes_a_stream_and_stream_classes_schedule_control_then_delta_then_bulk() {
        assert_eq!(TrafficClass::Audio.stream_priority(), None);
        let control = TrafficClass::Control.stream_priority().unwrap();
        let delta = TrafficClass::Delta.stream_priority().unwrap();
        let bulk = TrafficClass::Bulk.stream_priority().unwrap();
        assert!(control > delta && delta > bulk);
    }

    #[test]
    fn bulk_runs_at_full_link_rate_when_no_interactive_traffic_is_recent() {
        let mut pacer = BulkPacer::new(10.0 * 1024.0 * 1024.0);
        assert_eq!(pacer.admit(Duration::ZERO, 1024 * 1024), Duration::ZERO);
        assert_eq!(pacer.admit(Duration::ZERO, 1024 * 1024), Duration::from_millis(100));
    }

    #[test]
    fn bulk_drops_to_a_quarter_of_the_link_within_200_ms_of_interactive_traffic() {
        let mut pacer = BulkPacer::new(10.0 * 1024.0 * 1024.0);
        pacer.observe(TrafficClass::Audio, Duration::ZERO);
        assert_eq!(pacer.admit(Duration::from_millis(50), 1024 * 1024), Duration::from_millis(50));
        assert_eq!(pacer.admit(Duration::from_millis(50), 1024 * 1024), Duration::from_millis(50) + Duration::from_millis(400));
    }

    #[test]
    fn bulk_recovers_full_rate_after_the_busy_window_passes() {
        let mut pacer = BulkPacer::new(10.0 * 1024.0 * 1024.0);
        pacer.observe(TrafficClass::Control, Duration::ZERO);
        let later = Duration::from_millis(500);
        assert_eq!(pacer.admit(later, 1024 * 1024), later);
        assert_eq!(pacer.admit(later, 1024 * 1024), later + Duration::from_millis(100));
    }

    #[test]
    fn bulk_traffic_does_not_count_as_interactive() {
        let mut pacer = BulkPacer::new(10.0 * 1024.0 * 1024.0);
        pacer.observe(TrafficClass::Bulk, Duration::ZERO);
        assert_eq!(pacer.admit(Duration::from_millis(10), 1024 * 1024), Duration::from_millis(10));
        assert_eq!(pacer.admit(Duration::from_millis(10), 1024 * 1024), Duration::from_millis(110));
    }
}
