//! Wait-free single-producer, single-consumer PCM ring for the audio device callbacks (docs/implementation/04, section 2).
//!
//! The producer runs in the capture callback and the consumer in the render callback or the ASR thread. Neither side
//! takes a lock or allocates. Overflow drops the newest samples and counts them, so a stalled consumer never blocks
//! capture. A cancelled reply never renders again, even while the callback is reclaiming slots.

use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct PcmProducer {
    inner: Producer<f32>,
    dropped: Arc<AtomicU64>,
}

pub struct PcmConsumer {
    inner: Consumer<f32>,
    dropped: Arc<AtomicU64>,
}

/// Creates a ring that holds `capacity` samples.
pub fn pcm_ring(capacity: usize) -> (PcmProducer, PcmConsumer) {
    let (producer, consumer) = RingBuffer::new(capacity);
    let dropped = Arc::new(AtomicU64::new(0));
    (
        PcmProducer {
            inner: producer,
            dropped: dropped.clone(),
        },
        PcmConsumer {
            inner: consumer,
            dropped,
        },
    )
}

impl PcmProducer {
    /// Worker-side TTS backpressure: offer at most this many samples and retain the unwritten tail.
    #[cfg(feature = "native")]
    pub(crate) fn writable(&self) -> usize {
        self.inner.slots()
    }

    /// Writes as many samples as fit and returns how many were written. The rest are dropped and counted.
    pub fn push_slice(&mut self, samples: &[f32]) -> usize {
        let (written, lost) = self.inner.push_partial_slice(samples);
        if !lost.is_empty() {
            self.dropped.fetch_add(lost.len() as u64, Ordering::Release);
        }
        written.len()
    }

    /// Total samples dropped because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }
}

impl PcmConsumer {
    /// Fills `out` from the front of the ring and returns how many samples were read.
    pub fn pop_slice(&mut self, out: &mut [f32]) -> usize {
        self.inner.pop_partial_slice(out).0.len()
    }

    /// Dropped source samples mean capture and render are no longer time-aligned.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }

    /// Discards everything currently queued. Debug builds walk discarded slots;
    /// the realtime callback uses `discard_at_most` instead.
    pub fn flush(&mut self) -> usize {
        self.discard_at_most(usize::MAX)
    }

    pub(crate) fn discard_at_most(&mut self, limit: usize) -> usize {
        let available = self.inner.slots().min(limit);
        if available == 0 {
            return 0;
        }
        match self.inner.read_chunk(available) {
            Ok(chunk) => {
                chunk.commit_all();
                available
            }
            Err(_) => 0,
        }
    }

    pub fn available(&self) -> usize {
        self.inner.slots()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.inner.buffer().capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_arrive_in_order_across_threads() {
        let (mut tx, mut rx) = pcm_ring(1024);
        let total = 200_000usize;
        let producer = std::thread::spawn(move || {
            let mut next = 0usize;
            while next < total {
                let end = (next + 97).min(total);
                let chunk: Vec<f32> = (next..end).map(|i| i as f32).collect();
                let mut offset = 0;
                while offset < chunk.len() {
                    offset += tx.push_slice(&chunk[offset..]);
                    if offset < chunk.len() {
                        std::thread::yield_now();
                    }
                }
                next = end;
            }
        });
        let mut expected = 0usize;
        let mut buf = vec![0.0f32; 128];
        while expected < total {
            let n = rx.pop_slice(&mut buf);
            for &v in &buf[..n] {
                assert_eq!(v, expected as f32);
                expected += 1;
            }
            if n == 0 {
                std::thread::yield_now();
            }
        }
        producer.join().unwrap();
    }

    #[test]
    fn overflow_drops_newest_and_counts_them() {
        let (mut tx, mut rx) = pcm_ring(4);
        assert_eq!(tx.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 4);
        assert_eq!(tx.dropped(), 2);
        let mut out = [0.0f32; 8];
        assert_eq!(rx.pop_slice(&mut out), 4);
        assert_eq!(&out[..4], &[1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn flush_discards_queued_audio_and_frees_space() {
        let (mut tx, mut rx) = pcm_ring(8);
        tx.push_slice(&[0.5; 8]);
        assert_eq!(rx.flush(), 8);
        assert_eq!(rx.available(), 0);
        assert_eq!(tx.push_slice(&[0.25; 8]), 8);
        let mut out = [0.0f32; 8];
        assert_eq!(rx.pop_slice(&mut out), 8);
        assert!(out.iter().all(|&s| s == 0.25));
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn flushing_a_full_ring_takes_under_five_milliseconds() {
        let capacity = 1 << 20;
        let (mut tx, mut rx) = pcm_ring(capacity);
        let block = vec![0.1f32; 4096];
        while tx.push_slice(&block) == block.len() {}
        let queued = rx.available();
        assert!(queued > capacity / 2, "ring must be nearly full before the flush");

        let start = std::time::Instant::now();
        let dropped = rx.flush();
        let elapsed = start.elapsed();
        assert_eq!(dropped, queued);
        assert!(elapsed < std::time::Duration::from_millis(5), "flush took {elapsed:?}");
    }
}
