//! Host-only debugger bridge for the built-in fixtures. No target, PID,
//! expression, command line or address is accepted.

use crate::{owned_dap, owned_debugpy, owned_jvm};
pub use owned_dap::{Adapter, SourceFrame, Visit};

#[derive(Clone, Copy, Debug)]
pub enum Fixture {
    Native(Adapter, Visit),
    Python(Visit),
    /// Hit count two in the fixed OpenJDK fixture, using JDI over private JDWP pipes.
    Jvm,
}

/// Allocation/free totals, retained Python markers and JVM instance counts
/// measure different things. Do not compare them as heap sizes.
#[derive(Debug, Eq, PartialEq)]
pub enum HeapObservation {
    AllocationCounters(owned_dap::AllocationCounters),
    PythonMarkers(owned_debugpy::MarkerCounters),
    LiveInstances(owned_jvm::HeapCounts),
}

#[derive(Debug, Eq, PartialEq)]
pub struct Observation {
    pub frames: [SourceFrame; 3],
    pub heap: HeapObservation,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    Dap(owned_dap::Error),
    Jvm(owned_jvm::Error),
}

/// Run only the chosen backend. An unavailable adapter does not authorize a
/// different backend, direct attach, or a retry outside confinement.
pub fn inspect(fixture: Fixture) -> Result<Observation, Error> {
    match fixture {
        Fixture::Native(adapter, visit) => {
            let obs = owned_dap::observe_counting_fixture(adapter, visit).map_err(Error::Dap)?;
            Ok(Observation {
                frames: obs.frames,
                heap: HeapObservation::AllocationCounters(obs.counters),
            })
        }
        Fixture::Python(visit) => {
            let obs = owned_debugpy::observe_counting_fixture(visit).map_err(Error::Dap)?;
            Ok(Observation {
                frames: obs.frames,
                heap: HeapObservation::PythonMarkers(obs.counters),
            })
        }
        Fixture::Jvm => {
            let obs = owned_jvm::observe_fixture().map_err(Error::Jvm)?;
            Ok(Observation {
                frames: obs.frames.map(|f| SourceFrame {
                    function: f.method,
                    line: f.line,
                }),
                heap: HeapObservation::LiveInstances(obs.counts),
            })
        }
    }
}
