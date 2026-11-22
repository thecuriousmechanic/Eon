use eon_cyber::defense::{Assessment, Gap, telemetry::measure_self_user_cpu};

#[cfg(not(all(
    feature = "self-cpu-ebpf",
    any(target_os = "linux", target_os = "android")
)))]
#[test]
fn feature_off_still_runs_action_once_without_a_counter() {
    let mut calls = 0;
    let result = measure_self_user_cpu(|| {
        calls += 1;
        42
    });
    assert_eq!(calls, 1);
    assert_eq!(result, (42, Assessment::Unverified(Gap::Unsupported)));
}

#[cfg(all(
    feature = "self-cpu-ebpf",
    any(target_os = "linux", target_os = "android")
))]
mod supported {
    use super::*;
    use std::{fs, hint::black_box, sync::mpsc, thread};

    fn work() -> u64 {
        let mut n = 1u64;
        for _ in 0..15_000_000 {
            n = black_box(n.wrapping_mul(3).rotate_left(1));
        }
        n
    }

    fn fds() -> usize {
        fs::read_dir("/proc/self/fd").unwrap().count()
    }

    fn assert_fixture_permissions(effective: u64) {
        let status = fs::read_to_string("/proc/thread-self/status").unwrap();
        let field = |key| {
            status
                .lines()
                .find_map(|s| s.strip_prefix(key))
                .unwrap()
                .trim()
        };
        assert!(field("Uid:").split_ascii_whitespace().all(|uid| uid != "0"));
        assert_eq!(
            u64::from_str_radix(field("CapEff:"), 16).unwrap(),
            effective
        );
        assert_eq!(field("NoNewPrivs:"), "1");
    }

    #[test]
    #[ignore = "run separately as a non-root caller without capabilities"]
    fn unavailable_never_becomes_a_zero_measurement() {
        assert_fixture_permissions(0);
        let before = fds();
        let result = measure_self_user_cpu(|| 42);
        assert_eq!(result, (42, Assessment::Unverified(Gap::Unavailable)));
        assert_eq!(fds(), before);
    }

    #[test]
    #[ignore = "run separately on a supported kernel with only CAP_BPF and CAP_PERFMON"]
    fn counts_only_owner_userspace_and_releases_fds() {
        assert_fixture_permissions((1 << 38) | (1 << 39));
        let before = fds();
        let expected = work();
        for existing_thread in [true, false] {
            let (result, count) = measure_self_user_cpu(work);
            assert_eq!(result, expected);
            let Assessment::Observed(n) = count else {
                panic!("no measured owner samples: {count:?}");
            };
            assert!(n > 2, "expected several samples: {n}");
            eprintln!("owner CPU samples: {n}");

            let (result, idle) = if existing_thread {
                let (go, wait) = mpsc::sync_channel(0);
                let other = thread::spawn(move || {
                    wait.recv().unwrap();
                    work()
                });
                measure_self_user_cpu(|| {
                    go.send(()).unwrap();
                    other.join().unwrap()
                })
            } else {
                // A thread created *after* attachment must not inherit the event.
                measure_self_user_cpu(|| thread::spawn(work).join().unwrap())
            };
            assert_eq!(result, expected);
            assert_eq!(idle, Assessment::Unverified(Gap::MissingEvidence));
            assert_eq!(fds(), before);
        }
        // The unwind path also closes its event and map.
        assert!(std::panic::catch_unwind(|| measure_self_user_cpu(|| panic!("fixture"))).is_err());
        assert_eq!(fds(), before);
    }
}
