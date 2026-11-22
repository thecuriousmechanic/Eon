use super::{Assessment, Gap};

/// An explicitly started, process-local libc fsync entry counter. Drop to stop.
pub struct SelfFsyncSensor {
    #[cfg(all(feature = "self-fsync-ebpf", target_os = "linux"))]
    inner: eon_ebpf::self_fsync::SelfFsyncCounter,
    #[cfg(not(all(feature = "self-fsync-ebpf", target_os = "linux")))]
    _unsupported: std::convert::Infallible,
}

impl SelfFsyncSensor {
    pub fn start() -> Assessment<Self> {
        #[cfg(all(feature = "self-fsync-ebpf", target_os = "linux"))]
        {
            match eon_ebpf::self_fsync::SelfFsyncCounter::attach() {
                Ok(inner) => Assessment::Observed(Self { inner }),
                Err(_) => Assessment::Unverified(Gap::Unavailable),
            }
        }
        #[cfg(not(all(feature = "self-fsync-ebpf", target_os = "linux")))]
        Assessment::Unverified(Gap::Unsupported)
    }

    pub fn count(&self) -> Assessment<u64> {
        #[cfg(all(feature = "self-fsync-ebpf", target_os = "linux"))]
        {
            match self.inner.count() {
                Ok(count) => Assessment::Observed(count),
                Err(_) => Assessment::Unverified(Gap::Unavailable),
            }
        }
        #[cfg(not(all(feature = "self-fsync-ebpf", target_os = "linux")))]
        match self._unsupported {}
    }
}
