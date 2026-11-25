//! Zero-allocation check for the audio ring (docs/implementation/04, section 6, `rt_callback_allocation_test`).
//!
//! A counting global allocator tracks allocations made on the test thread while armed. The ring's push, pop, and
//! flush paths must not allocate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use eon_voice::barge_in::BargeIn;
use eon_voice::ring::pcm_ring;

struct CountingAlloc;

// Thread-local so that allocations on other test threads (tests run in parallel) cannot leak into this count.
thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.with(Cell::get) {
            ALLOCATIONS.with(|count| count.set(count.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn allocations_during(f: impl FnOnce()) -> usize {
    ALLOCATIONS.with(|count| count.set(0));
    ARMED.with(|armed| armed.set(true));
    f();
    ARMED.with(|armed| armed.set(false));
    ALLOCATIONS.with(Cell::get)
}

#[test]
fn counter_sees_allocations_so_the_zero_result_is_meaningful() {
    let allocs = allocations_during(|| {
        let v: Vec<u8> = std::hint::black_box(vec![7u8; 64]);
        std::hint::black_box(v);
    });
    assert!(allocs > 0, "allocation counter is not observing the armed window");
}

#[test]
fn callback_path_does_not_allocate() {
    let (mut tx, mut rx) = pcm_ring(4096);
    let frame = [0.25f32; 160];
    let mut out = [0.0f32; 160];
    let signal = BargeIn::new();

    // Warm up outside the armed window so any one-time setup is excluded.
    tx.push_slice(&frame);
    signal.render(&mut rx, &mut out);

    let allocs = allocations_during(|| {
        for _ in 0..10_000 {
            tx.push_slice(&frame);
            rx.pop_slice(&mut out);
        }
        tx.push_slice(&frame);
        signal.trigger();
        assert!(signal.render(&mut rx, &mut out).acknowledged);
        tx.push_slice(&frame);
        assert!(signal.render(&mut rx, &mut out).muted);
    });
    assert_eq!(allocs, 0, "ring callback path allocated {allocs} times");
}
