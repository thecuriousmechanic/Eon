#![cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_pointer_width = "64"
))]

use eon_cyber::dev_dap::{Adapter, Fixture, HeapObservation, Visit, inspect};
use eon_cyber::{
    owned_dap::AllocationCounters, owned_debugpy::MarkerCounters, owned_jvm::HeapCounts,
};

fn check(fixture: Fixture, expected_names: [&str; 3], expected_heap: HeapObservation) {
    let obs = inspect(fixture).unwrap_or_else(|e| panic!("{fixture:?}: {e:?}"));
    assert_eq!(obs.frames.each_ref().map(|f| f.function), expected_names);
    assert!(obs.frames.iter().all(|f| f.line > 0));
    assert_eq!(obs.heap, expected_heap);
}

#[test]
#[ignore = "requires provisioned CodeLLDB and LLVM"]
fn native_fixture_reports_allocation_counters() {
    check(
        Fixture::Native(Adapter::CodeLldb, Visit::Third),
        ["count", "third", "main"],
        HeapObservation::AllocationCounters(AllocationCounters {
            allocations: 3,
            frees: 2,
        }),
    );
}

#[test]
#[ignore = "requires pinned debugpy and private unprivileged namespaces"]
fn python_fixture_reports_retained_markers() {
    check(
        Fixture::Python(Visit::Second),
        ["count", "second", "main"],
        HeapObservation::PythonMarkers(MarkerCounters {
            allocations: 2,
            live: 1,
        }),
    );
}

#[test]
#[ignore = "requires the pinned Temurin JDK"]
fn jvm_fixture_reports_live_instances() {
    check(
        Fixture::Jvm,
        ["mark", "selected", "main"],
        HeapObservation::LiveInstances(HeapCounts {
            owned_fixture: 0,
            marker: 3,
        }),
    );
}
