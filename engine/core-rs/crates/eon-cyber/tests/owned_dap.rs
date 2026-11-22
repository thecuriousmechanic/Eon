#![cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_pointer_width = "64"
))]

use eon_cyber::owned_dap::{Adapter, AllocationCounters, Visit, observe_counting_fixture};
use std::sync::Mutex;

static PROBE: Mutex<()> = Mutex::new(());

#[test]
#[ignore = "requires provisioned LLVM 18 clang, lldb-dap, lldb-server and local child tracing"]
fn real_lldb_dap_hits_only_the_selected_call() {
    check_visits(Adapter::LldbDap);
}

#[test]
#[ignore = "requires provisioned CodeLLDB v1.12.3, LLVM 18 and local child tracing"]
fn real_codelldb_hits_only_the_selected_call() {
    check_visits(Adapter::CodeLldb);
}

fn check_visits(adapter: Adapter) {
    let _run = PROBE.lock().unwrap();
    for (visit, caller, caller_line, main_line, allocations, frees) in [
        (Visit::Second, "second", 20, 45, 2, 1),
        (Visit::Third, "third", 24, 48, 3, 2),
    ] {
        let seen = observe_counting_fixture(adapter, visit).expect("fixed counting fixture");
        assert_eq!(
            seen.frames.each_ref().map(|f| (f.function, f.line)),
            [("count", 12), (caller, caller_line), ("main", main_line)]
        );
        assert_eq!(seen.counters, AllocationCounters { allocations, frees });
        println!("{adapter:?} {visit:?}: {seen:?}");
    }
}
