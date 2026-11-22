use super::{Assessment, Gap};

/// Count sampled userspace CPU-clock overflows on the calling thread during a
/// synchronous action. Not CPU time, cycles, precise attribution, or a stack trace.
/// The action still runs exactly once when no measurement is possible.
pub fn measure_self_user_cpu<T>(action: impl FnOnce() -> T) -> (T, Assessment<u64>) {
    #[cfg(all(
        feature = "self-cpu-ebpf",
        any(target_os = "linux", target_os = "android")
    ))]
    {
        let counter = native::Counter::begin();
        let value = action();
        (
            value,
            counter.map_or(Assessment::Unverified(Gap::Unavailable), |c| c.finish()),
        )
    }
    #[cfg(not(all(
        feature = "self-cpu-ebpf",
        any(target_os = "linux", target_os = "android")
    )))]
    (action(), Assessment::Unverified(Gap::Unsupported))
}

#[cfg(all(
    feature = "self-cpu-ebpf",
    any(target_os = "linux", target_os = "android")
))]
mod native {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    unsafe extern "C" {
        fn eon_self_cpu_begin(fds: *mut i32) -> i32;
        fn eon_self_cpu_disable(event: i32) -> i32;
        fn eon_self_cpu_read(map: i32, samples: *mut u64) -> i32;
    }

    pub(super) struct Counter {
        event: OwnedFd,
        map: OwnedFd,
        owner: u32,
        active: bool,
    }

    impl Counter {
        pub(super) fn begin() -> Option<Self> {
            let mut fds = [-1; 2];
            // SAFETY: this fixed function transfers two owned fds only on success.
            if unsafe { eon_self_cpu_begin(fds.as_mut_ptr()) } != 0 {
                return None;
            }
            Some(Self {
                // SAFETY: separate freshly acquired fds, neither closed nor shared with Rust.
                event: unsafe { OwnedFd::from_raw_fd(fds[0]) },
                map: unsafe { OwnedFd::from_raw_fd(fds[1]) },
                owner: std::process::id(),
                active: true,
            })
        }

        fn stop(&mut self) -> bool {
            // A forked child must not stop or read its parent's counter.
            if self.owner != std::process::id() {
                return false;
            }
            // SAFETY: self owns the initialized perf event.
            if unsafe { eon_self_cpu_disable(self.event.as_raw_fd()) } != 0 {
                return false;
            }
            self.active = false;
            true
        }

        pub(super) fn finish(mut self) -> Assessment<u64> {
            let mut samples = 0;
            // SAFETY: read writes only a u64, from key zero in the fixed map.
            if !self.stop() || unsafe { eon_self_cpu_read(self.map.as_raw_fd(), &mut samples) } != 0
            {
                Assessment::Unverified(Gap::Unavailable)
            } else if samples == 0 {
                Assessment::Unverified(Gap::MissingEvidence)
            } else {
                Assessment::Observed(samples)
            }
        }
    }

    impl Drop for Counter {
        fn drop(&mut self) {
            if self.active {
                self.stop();
            }
        }
    }
}
