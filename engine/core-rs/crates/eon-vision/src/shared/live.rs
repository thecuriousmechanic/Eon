use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{SharedJpeg, SharedVisualMemory};
use crate::keyframes::Luma;
use crate::{EonError, EonResult, VideoKeyframe};

struct State {
    history: SharedVisualMemory,
    stopping: bool,
}

struct Inner {
    origin: Instant,
    state: Mutex<State>,
    changed: Condvar,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // Sample under the state lock so observations, selections and expiry are ordered.
    fn now_ms(&self) -> u64 {
        self.origin
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    fn expire_when_due(&self) {
        let mut state = self.lock();
        while !state.stopping {
            state
                .history
                .advance(self.now_ms())
                .expect("monotonic live clock");
            state = match state.history.next_expiry_ms() {
                Some(end) => {
                    let delay = Duration::from_millis(end).saturating_sub(self.origin.elapsed());
                    self.changed
                        .wait_timeout(state, delay)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }
}

/// Producer-owned history for live capture. One joinable worker sleeps until the
/// oldest frame's 30-second deadline, or until notified. Timestamps are monotonic
/// milliseconds since construction. Expiry is subject to OS scheduling.
///
/// The byte budget and expiry apply to the history's descriptors, not independent
/// copies obtained with [`select`](Self::select). Dropping or stopping the history
/// wakes and joins its worker and closes its remaining descriptors before returning.
pub struct LiveSharedVisualMemory {
    inner: Arc<Inner>,
    worker: Option<JoinHandle<()>>,
}

impl LiveSharedVisualMemory {
    /// Start a history with the portable history's default byte budget.
    pub fn new() -> EonResult<Self> {
        Self::start(SharedVisualMemory::default())
    }

    pub fn with_budget(bytes: usize) -> EonResult<Self> {
        Self::start(SharedVisualMemory::with_budget(bytes))
    }

    fn start(history: SharedVisualMemory) -> EonResult<Self> {
        let inner = Arc::new(Inner {
            origin: Instant::now(),
            state: Mutex::new(State {
                history,
                stopping: false,
            }),
            changed: Condvar::new(),
        });
        let timer = Arc::clone(&inner);
        let worker = thread::Builder::new()
            .name("eon-vision-expiry".into())
            .spawn(move || timer.expire_when_due())
            .map_err(|e| EonError::OperationFailed {
                operation: "start keyframe expiry".into(),
                reason: e.to_string(),
            })?;
        Ok(Self {
            inner,
            worker: Some(worker),
        })
    }

    /// Sample an already-authorized capture. Returns its sequence number if accepted.
    pub fn observe(
        &self,
        dimensions: (u32, u32),
        thumbnail: &Luma,
        jpeg: &[u8],
    ) -> EonResult<Option<u64>> {
        let mut state = self.inner.lock();
        let seq = state
            .history
            .observe(self.inner.now_ms(), dimensions, thumbnail, jpeg)?;
        if seq.is_some() {
            self.inner.changed.notify_one();
        }
        Ok(seq)
    }

    /// List retained sequence numbers, oldest first, without duplicating descriptors.
    pub fn recent(&self) -> EonResult<Vec<u64>> {
        let mut state = self.inner.lock();
        Ok(state
            .history
            .recent(0, self.inner.now_ms())?
            .map(|f| f.sequence_number)
            .collect())
    }

    /// Duplicate just the selected frame for the capture owner, or return `None` if
    /// no longer retained. Using the result never holds the history lock.
    pub fn select(&self, sequence: u64) -> EonResult<Option<VideoKeyframe<SharedJpeg>>> {
        let mut state = self.inner.lock();
        let mut frames = state.history.recent(0, self.inner.now_ms())?;
        let Some(f) = frames.find(|f| f.sequence_number == sequence) else {
            return Ok(None);
        };
        let file = f
            .jpeg_bytes
            .0
            .try_clone()
            .map_err(|e| EonError::OperationFailed {
                operation: "duplicate selected keyframe".into(),
                reason: e.to_string(),
            })?;
        Ok(Some(VideoKeyframe {
            sequence_number: f.sequence_number,
            timestamp_ms: f.timestamp_ms,
            width: f.width,
            height: f.height,
            is_scene_transition: f.is_scene_transition,
            jpeg_bytes: SharedJpeg(file),
        }))
    }

    /// Synchronously stop capture history maintenance; equivalent to dropping `self`.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for LiveSharedVisualMemory {
    fn drop(&mut self) {
        self.inner.lock().stopping = true;
        self.inner.changed.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::ErrorKind;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    // Inspect only our own known descriptors. The number alone could be reused
    // by another parallel test. Neither the probe nor its metadata keeps a file open.
    struct FdProbe {
        path: String,
        identity: (u64, u64),
    }

    impl FdProbe {
        fn producer(live: &LiveSharedVisualMemory) -> Self {
            let state = live.inner.lock();
            let file = &state.history.retained().next().unwrap().jpeg_bytes.0;
            Self::of(file)
        }

        fn of(file: &fs::File) -> Self {
            let meta = file.metadata().unwrap();
            Self {
                path: format!("/proc/self/fd/{}", file.as_raw_fd()),
                identity: (meta.dev(), meta.ino()),
            }
        }

        fn still_open(&self) -> bool {
            match fs::metadata(&self.path) {
                Ok(meta) => self.identity == (meta.dev(), meta.ino()),
                Err(e) if e.kind() == ErrorKind::NotFound => false,
                Err(e) => panic!("checking descriptor: {e}"),
            }
        }
    }

    #[test]
    fn idle_worker_closes_the_producer_descriptor_without_another_history_call() {
        let live = LiveSharedVisualMemory::start(SharedVisualMemory::with_limits(32, 500)).unwrap();
        assert_eq!(live.observe((1, 1), &[0; 1024], b"a").unwrap(), Some(1));
        let fd = FdProbe::producer(&live);
        assert!(fd.still_open());

        // No recent/select/observe, advancing test clock, or history lock in this loop.
        let deadline = Instant::now() + Duration::from_secs(5);
        while fd.still_open() {
            assert!(Instant::now() < deadline, "idle producer fd did not close");
            thread::sleep(Duration::from_millis(10));
        }
        // Keep the live owner in scope until after the kernel confirms closure.
        assert!(live.select(1).unwrap().is_none());
        assert_eq!(live.observe((1, 1), &[0; 1024], b"b").unwrap(), Some(2));
    }

    #[test]
    fn stop_and_drop_release_worker_state_and_unexpired_files() {
        for explicit_stop in [true, false] {
            let live = LiveSharedVisualMemory::new().unwrap();
            let state = Arc::downgrade(&live.inner);
            let seq = live.observe((1, 1), &[0; 1024], b"a").unwrap().unwrap();
            let fd = FdProbe::producer(&live);
            let selected = live.select(seq).unwrap().unwrap();
            let copy = FdProbe::of(&selected.jpeg_bytes.0);
            assert!(fd.still_open());
            if explicit_stop {
                live.stop();
            } else {
                drop(live);
            }
            assert!(state.upgrade().is_none());
            assert!(!fd.still_open());
            assert!(copy.still_open());
            drop(selected);
            assert!(!copy.still_open());
        }
    }
}
