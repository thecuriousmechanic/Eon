#![cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_pointer_width = "64"
))]

use eon_cyber::owned_jvm::{HeapCounts, observe_fixture};

#[test]
#[ignore = "requires pinned Linux x64 Temurin 17.0.21+9 local profile"]
fn real_jdi_counts_at_only_the_second_visit() {
    let observation = observe_fixture().expect("fixed JDI probe");
    assert_eq!(
        observation.frames.map(|frame| (frame.method, frame.line)),
        [("mark", 7), ("selected", 18), ("main", 23)]
    );
    assert_eq!(
        observation.counts,
        HeapCounts {
            owned_fixture: 0,
            marker: 3
        }
    );
    println!("{observation:?}");
}
