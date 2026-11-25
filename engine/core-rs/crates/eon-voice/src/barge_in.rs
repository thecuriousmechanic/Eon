//! Barge-in signal shared between the speech detector and the playback callback (docs/implementation/04, section 3).
//!
//! When the user starts speaking over the assistant, the detector calls `trigger`. The next playback callback sees the
//! request and writes silence. Queued and late TTS are discarded in bounded batches. The detector never touches the ring itself,
//! so it never blocks the audio thread.

use std::future::poll_fn;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Poll, Waker};

use crate::ring::PcmConsumer;

// Even: idle, odd: playing. A new run gets a new number, even if it starts and
// stops between two observations on the inference worker. The top two are terminal.
const REQUESTED: u64 = u64::MAX - 1;
const MUTED: u64 = u64::MAX;

fn is_running(state: u64) -> bool {
    state < REQUESTED && state & 1 != 0
}

#[cfg(feature = "native")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlaybackRun(u64);

#[cfg(feature = "native")]
impl PlaybackRun {
    pub(crate) const FIRST: Self = Self(1);
}

// rtrb's discard walks slots in debug builds. Silencing must never depend on the queue depth.
const DISCARD_BUDGET: usize = 1024;

/// One reply's playback signal. A cancelled reply cannot resume (including late producer writes).
/// Start a fresh ring and signal for the next reply.
#[derive(Debug, Clone, Default)]
pub struct BargeIn {
    inner: Arc<Signal>,
}

#[derive(Debug, Default)]
struct Signal {
    state: AtomicU64,
    // Not on the render callback path. Serializes an individual transport
    // submission poll with revocation; never held across an async wait.
    observer: Mutex<Option<Observer>>,
}

#[derive(Debug, Default)]
struct Observer {
    waker: Option<Waker>,
}

/// One wakeable readback per reply. Dropping its future also releases the
/// registered waker, even if the worker or transport has made no progress.
pub struct Interruption<'a>(&'a BargeIn);

impl Interruption<'_> {
    pub async fn wait(&mut self) {
        poll_fn(|cx| {
            let mut slot = self.0.observer();
            if self.0.is_muted() {
                return Poll::Ready(());
            }
            let observer = slot.as_mut().expect("live interruption registration");
            if let Some(old) = &mut observer.waker {
                old.clone_from(cx.waker());
            } else {
                observer.waker = Some(cx.waker().clone());
            }
            Poll::Pending
        })
        .await
    }
}

impl Drop for Interruption<'_> {
    fn drop(&mut self) {
        *self.0.observer() = None;
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    pub samples: usize,
    pub flushed: usize,
    pub acknowledged: bool,
    pub muted: bool,
}

impl BargeIn {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels unconditionally. Only the first request needs acknowledgment.
    pub fn trigger(&self) {
        self.revoke(|state| state.fetch_max(REQUESTED, Ordering::AcqRel) < REQUESTED);
    }

    #[cfg(feature = "native")]
    pub(crate) fn interrupt_playing(&self, run: PlaybackRun) -> bool {
        self.revoke(|state| {
            state
                .compare_exchange(run.0, REQUESTED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        })
    }

    fn observer(&self) -> MutexGuard<'_, Option<Observer>> {
        self.inner.observer.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn revoke(&self, transition: impl FnOnce(&AtomicU64) -> bool) -> bool {
        let waker = {
            let mut slot = self.observer();
            if !transition(&self.inner.state) {
                return false;
            }
            slot.as_mut().and_then(|o| o.waker.take())
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        true
    }

    pub fn watch_interruption(&self) -> Option<Interruption<'_>> {
        let mut slot = self.observer();
        if self.is_muted() || slot.is_some() {
            return None;
        }
        *slot = Some(Observer::default());
        Some(Interruption(self))
    }

    /// Linearize **one nonblocking transport submission** with both ways to
    /// revoke a reply. Never wait, loop over fragments, or re-enter the signal
    /// inside `submit`. An accepted fragment cannot be retracted.
    pub fn submit_if_live<T>(&self, submit: impl FnOnce() -> T) -> Option<T> {
        let _guard = self.observer();
        (!self.is_muted()).then(submit)
    }

    /// Acknowledges the request without unmuting the reply.
    pub fn take(&self) -> bool {
        self.inner
            .state
            .compare_exchange(REQUESTED, MUTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn is_pending(&self) -> bool {
        self.inner.state.load(Ordering::Acquire) == REQUESTED
    }

    pub fn is_muted(&self) -> bool {
        self.inner.state.load(Ordering::Acquire) >= REQUESTED
    }

    pub fn is_playing(&self) -> bool {
        is_running(self.inner.state.load(Ordering::Acquire))
    }

    #[cfg(feature = "native")]
    pub(crate) fn playing_run(&self) -> Option<PlaybackRun> {
        let state = self.inner.state.load(Ordering::Acquire);
        is_running(state).then_some(PlaybackRun(state))
    }

    fn set_playing(&self, playing: bool) {
        let old = self.inner.state.load(Ordering::Acquire);
        if old < REQUESTED && (old & 1 != 0) != playing {
            // A concurrent revocation always wins. No locks or retry on the audio thread.
            let _ = self.inner.state.compare_exchange(old, old + 1, Ordering::AcqRel, Ordering::Acquire);
        }
    }

    #[cfg(feature = "native")]
    pub(crate) fn playback_stopped(&self) {
        self.set_playing(false);
    }

    /// The mono portion of the real render callback, also usable by non-CPAL drivers.
    pub fn render(&self, consumer: &mut PcmConsumer, out: &mut [f32]) -> Rendered {
        let samples = if self.is_muted() { 0 } else { consumer.pop_slice(out) };
        self.set_playing(samples > 0);
        if self.is_muted() {
            out.fill(0.0);
            return Rendered {
                samples: 0,
                flushed: samples + consumer.discard_at_most(DISCARD_BUDGET),
                acknowledged: self.take(),
                muted: true,
            };
        }
        out[samples..].fill(0.0);
        Rendered {
            samples,
            ..Rendered::default()
        }
    }
}

/// Fires once after `required_frames` consecutive frames whose RMS is at or above `threshold_rms`. Run it on the
/// echo-cancelled capture signal, so the assistant's own voice does not count as the user speaking.
#[derive(Debug, Clone)]
pub struct SpeechOnsetDetector {
    threshold_rms: f32,
    required_frames: usize,
    run: usize,
    fired: bool,
}

impl SpeechOnsetDetector {
    pub fn new(threshold_rms: f32, required_frames: usize) -> Self {
        Self {
            threshold_rms,
            required_frames: required_frames.max(1),
            run: 0,
            fired: false,
        }
    }

    /// Returns true only on the frame that completes a qualifying run. Later frames return false until `reset`.
    pub fn push_frame(&mut self, frame: &[f32]) -> bool {
        if self.fired || frame.is_empty() {
            return false;
        }
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
        if rms >= self.threshold_rms {
            self.run += 1;
            if self.run >= self.required_frames {
                self.fired = true;
                return true;
            }
        } else {
            self.run = 0;
        }
        false
    }

    pub fn reset(&mut self) {
        self.run = 0;
        self.fired = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_backlog_is_immediately_inaudible_but_reclaimed_in_bounded_batches() {
        let (mut tx, mut rx) = crate::ring::pcm_ring(8 * DISCARD_BUDGET);
        tx.push_slice(&[0.8; 8 * DISCARD_BUDGET]);
        let barge = BargeIn::new();
        let mut out = [0.0; 8];
        barge.render(&mut rx, &mut out);
        barge.trigger();
        for i in 0..8 {
            let result = barge.render(&mut rx, &mut out);
            assert!(result.muted && result.flushed <= DISCARD_BUDGET);
            assert_eq!(result.acknowledged, i == 0);
            assert_eq!(out, [0.0; 8]);
        }
        assert_eq!(rx.available(), 0);
    }

    #[test]
    fn a_trigger_is_taken_exactly_once() {
        let barge = BargeIn::new();
        assert!(!barge.take());
        barge.trigger();
        barge.trigger();
        assert!(barge.is_pending());
        assert!(barge.take());
        assert!(!barge.take());
    }

    #[test]
    fn clones_share_the_same_request() {
        let detector = BargeIn::new();
        let playback = detector.clone();
        detector.trigger();
        assert!(playback.take());
    }

    #[test]
    fn onset_needs_a_sustained_run_above_the_threshold() {
        let loud = vec![0.5f32; 160];
        let quiet = vec![0.001f32; 160];
        let mut detector = SpeechOnsetDetector::new(0.1, 3);
        assert!(!detector.push_frame(&loud));
        assert!(!detector.push_frame(&loud));
        assert!(!detector.push_frame(&quiet), "a dip resets the run");
        assert!(!detector.push_frame(&loud));
        assert!(!detector.push_frame(&loud));
        assert!(detector.push_frame(&loud), "third consecutive loud frame fires");
        assert!(!detector.push_frame(&loud), "fires once per utterance");
        detector.reset();
        assert!(!detector.push_frame(&loud));
    }
}
