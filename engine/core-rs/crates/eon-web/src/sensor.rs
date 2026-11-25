//! Optional eBPF counter of this process's own fsync calls. Attaching needs root or CAP_BPF and a
//! kernel that allows the probe, so every failure is kept and shown rather than treated as fatal.

use eon_ebpf::self_fsync::{SelfFsyncCounter, SensorError};
use serde::Serialize;

#[derive(Default)]
pub struct Sensor {
    counter: Option<SelfFsyncCounter>,
    last_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SensorView {
    pub attached: bool,
    pub fsync_calls: Option<u64>,
    pub last_error: Option<String>,
}

impl Sensor {
    pub fn attach(&mut self) {
        match SelfFsyncCounter::attach() {
            Ok(counter) => {
                self.counter = Some(counter);
                self.last_error = None;
            }
            Err(error) => {
                self.counter = None;
                self.last_error = Some(describe(&error));
            }
        }
    }

    pub fn detach(&mut self) {
        self.counter = None;
        self.last_error = None;
    }

    pub fn view(&self) -> SensorView {
        let fsync_calls = self.counter.as_ref().and_then(|c| c.count().ok());
        SensorView {
            attached: self.counter.is_some(),
            fsync_calls,
            last_error: self.last_error.clone(),
        }
    }
}

fn describe(error: &SensorError) -> String {
    match error {
        SensorError::Library => "could not resolve the libc fsync mapping".into(),
        SensorError::Load => "the eBPF program failed to load (needs root or CAP_BPF)".into(),
        SensorError::Map => "the eBPF counter map is unavailable".into(),
        SensorError::Attach => "the eBPF probe could not attach to fsync".into(),
        SensorError::NotOwner => "the fsync mapping is not owned by this process".into(),
    }
}
