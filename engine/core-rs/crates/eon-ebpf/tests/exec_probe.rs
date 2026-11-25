//! Attaches the execve probe and checks that it counts real program launches. Needs root (CAP_BPF) and the object built by
//! `scripts/build_bpf.sh`. Run with the built test binary under sudo; see the test's ignore reason.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

use eon_ebpf::ExecCounter;

#[test]
#[ignore = "needs CAP_BPF: build with cargo test --no-run, then run the printed binary with sudo and EON_BPF_OBJ set"]
fn the_probe_counts_every_program_launch() {
    let object = PathBuf::from(std::env::var("EON_BPF_OBJ").expect("set EON_BPF_OBJ to the built probe object"));
    let bytes = std::fs::read(&object).expect("the probe object must be readable");
    let counter = ExecCounter::attach(&bytes).expect("the probe must load and attach");

    let before = counter.count().unwrap();
    for _ in 0..5 {
        let status = Command::new("/bin/true").status().unwrap();
        assert!(status.success());
    }
    let after = counter.count().unwrap();

    assert!(after >= before + 5, "expected at least 5 new execve calls, counted {} -> {}", before, after);
}
