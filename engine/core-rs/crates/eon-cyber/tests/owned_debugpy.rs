#![cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_pointer_width = "64"
))]

use eon_cyber::owned_dap::Visit;
use eon_cyber::owned_debugpy::inspect_counting_fixture;

#[test]
#[ignore = "requires private unprivileged namespaces and pinned debugpy wheel"]
fn real_debugpy_stops_at_each_selected_call_and_exits() {
    for (visit, caller, line, main) in [
        (Visit::Second, "second", 9, 16),
        (Visit::Third, "third", 12, 17),
    ] {
        let stack = inspect_counting_fixture(visit).expect("fixed Python fixture");
        assert_eq!(
            stack.each_ref().map(|f| (f.function, f.line)),
            [("count", 3), (caller, line), ("main", main)]
        );
        println!("{visit:?}: {stack:?}");
    }
}
