//! Self-only telemetry. Procfs storage-byte counters can include waited-for children;
//! the separately opted-in eBPF sensor counts only this process's libc fsync entries.

use super::{Assessment, Gap};
use std::time::{Duration, Instant};

mod review_etw;
mod self_cpu;
mod self_fsync;
pub use review_etw::submit_review_completion;
pub use self_cpu::measure_self_user_cpu;
pub use self_fsync::SelfFsyncSensor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageBytes {
    pub read: u64,
    pub written: u64,
}

#[derive(Debug)]
pub struct ProcessIo {
    pid: u32,
    at: Instant,
    bytes: StorageBytes,
}

impl ProcessIo {
    pub fn bytes(&self) -> StorageBytes {
        self.bytes
    }

    /// Compare two samples from this process. A reset or reordered observation is
    /// missing evidence, not zero activity or a wrap to a large positive value.
    pub fn change_since(&self, earlier: &Self) -> Assessment<(Duration, StorageBytes)> {
        if self.pid != earlier.pid {
            return Assessment::Unverified(Gap::Ambiguous);
        }
        match (
            self.at.checked_duration_since(earlier.at),
            self.bytes.read.checked_sub(earlier.bytes.read),
            self.bytes.written.checked_sub(earlier.bytes.written),
        ) {
            (Some(dt), Some(read), Some(written)) => {
                Assessment::Observed((dt, StorageBytes { read, written }))
            }
            _ => Assessment::Unverified(Gap::InvalidInput),
        }
    }
}

#[cfg(any(test, target_os = "linux", target_os = "android"))]
mod procfs {
    use super::*;
    use std::io::Read;

    const LIMIT: u64 = 4096;

    pub(super) fn read(input: impl Read) -> Result<StorageBytes, Gap> {
        let mut buf = Vec::new();
        input
            .take(LIMIT + 1)
            .read_to_end(&mut buf)
            .map_err(|_| Gap::Unavailable)?;
        if buf.len() as u64 > LIMIT {
            return Err(Gap::LimitExceeded);
        }
        let mut read = None;
        let mut written = None;
        for line in std::str::from_utf8(&buf)
            .map_err(|_| Gap::InvalidInput)?
            .lines()
        {
            let Some((key, raw)) = line.split_once(':') else {
                return Err(Gap::InvalidInput);
            };
            let slot = match key {
                "read_bytes" => &mut read,
                "write_bytes" => &mut written,
                _ => continue,
            };
            if slot.is_some() {
                return Err(Gap::Ambiguous);
            }
            let n = raw.trim_ascii();
            if !n.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Gap::InvalidInput);
            }
            *slot = Some(n.parse::<u64>().map_err(|_| Gap::InvalidInput)?);
        }
        Ok(StorageBytes {
            read: read.ok_or(Gap::MissingEvidence)?,
            written: written.ok_or(Gap::MissingEvidence)?,
        })
    }
}

/// Never accepts a target PID, path, command or network endpoint. A system without
/// access to procfs is reported as unverified, not as having no I/O activity.
pub fn sample_process_io() -> Assessment<ProcessIo> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let result = std::fs::File::open("/proc/self/io")
            .map_err(|_| Gap::Unavailable)
            .and_then(procfs::read);
        match result {
            Ok(bytes) => Assessment::Observed(ProcessIo {
                pid: std::process::id(),
                at: Instant::now(),
                bytes,
            }),
            Err(gap) => Assessment::Unverified(gap),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    Assessment::Unverified(Gap::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_storage_counters() {
        let bytes =
            procfs::read(&b"rchar: 8765\nwrite_bytes: 4096\nsyscr: 21\nread_bytes: 0\n"[..])
                .unwrap();
        assert_eq!(
            bytes,
            StorageBytes {
                read: 0,
                written: 4096
            }
        );
    }

    #[test]
    fn unknown_duplicate_malformed_and_oversized_inputs_do_not_report_counters() {
        for (src, gap) in [
            ("wchar: 0\nread_bytes: 0\n", Gap::MissingEvidence),
            (
                "read_bytes: 0\nread_bytes: 0\nwrite_bytes: 0\n",
                Gap::Ambiguous,
            ),
            ("read_bytes: -42\nwrite_bytes: 0\n", Gap::InvalidInput),
            (
                "read_bytes: 18446744073709551616\nwrite_bytes: 0\n",
                Gap::InvalidInput,
            ),
            (
                "read_bytes: private-fixture-not-a-counter\n",
                Gap::InvalidInput,
            ),
        ] {
            assert_eq!(procfs::read(src.as_bytes()), Err(gap));
        }
        assert_eq!(procfs::read(std::io::repeat(0)), Err(Gap::LimitExceeded));
    }

    #[test]
    fn correlation_rejects_time_pid_and_counter_discontinuities() {
        let start = Instant::now();
        let dt = Duration::from_millis(12);
        let sample = |pid, at, read, written| ProcessIo {
            pid,
            at,
            bytes: StorageBytes { read, written },
        };
        let a = sample(10, start, 24, 0);
        let b = sample(10, start + dt, 28, 12);
        assert_eq!(
            b.change_since(&a),
            Assessment::Observed((
                dt,
                StorageBytes {
                    read: 4,
                    written: 12
                }
            ))
        );
        assert_eq!(
            a.change_since(&b),
            Assessment::Unverified(Gap::InvalidInput)
        );
        assert_eq!(
            sample(10, start + dt, 23, 8).change_since(&a),
            Assessment::Unverified(Gap::InvalidInput)
        );
        assert_eq!(
            sample(11, start + dt, 28, 12).change_since(&a),
            Assessment::Unverified(Gap::Ambiguous)
        );
    }
}
