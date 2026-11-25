use eon_cyber::defense::{Assessment, Gap, telemetry::SelfFsyncSensor};

#[cfg(not(all(target_os = "linux", feature = "self-fsync-ebpf")))]
#[test]
fn feature_off_is_not_a_zero_count() {
    assert!(matches!(
        SelfFsyncSensor::start(),
        Assessment::Unverified(Gap::Unsupported)
    ));
}

#[cfg(all(target_os = "linux", feature = "self-fsync-ebpf"))]
mod linux {
    use super::*;
    use std::{env, fs, process::Command, thread};

    const CALLS: u64 = 5;

    fn sync_own_fixture() {
        let file = fs::File::open(env::current_exe().unwrap()).unwrap();
        for _ in 0..CALLS {
            file.sync_all().unwrap();
        }
    }

    fn field<'a>(status: &'a str, name: &str) -> &'a str {
        status
            .lines()
            .find_map(|s| s.strip_prefix(name))
            .unwrap()
            .trim()
    }

    fn assert_nonroot(status: &str) {
        assert!(
            field(status, "Uid:")
                .split_ascii_whitespace()
                .all(|n| n != "0")
        );
    }

    fn assert_unprivileged() {
        let status = fs::read_to_string("/proc/self/status").unwrap();
        assert_nonroot(&status);
        for name in ["CapInh:", "CapEff:", "CapPrm:", "CapAmb:"] {
            assert_eq!(u64::from_str_radix(field(&status, name), 16).unwrap(), 0);
        }
        assert_eq!(field(&status, "NoNewPrivs:"), "1");
    }

    #[test]
    #[ignore = "fixture for the process-isolation test, no sensor or privileges"]
    fn unprivileged_child() {
        assert_unprivileged();
        sync_own_fixture();
    }

    #[test]
    #[ignore = "run separately as a non-root user without capabilities on a restricted kernel"]
    fn permission_denial_is_unverified() {
        assert_unprivileged();
        use eon_ebpf::self_fsync::{SelfFsyncCounter, SensorError};
        assert_eq!(SelfFsyncCounter::attach().err(), Some(SensorError::Load));
        assert!(matches!(
            SelfFsyncSensor::start(),
            Assessment::Unverified(Gap::Unavailable)
        ));
    }

    #[test]
    #[ignore = "explicit native uprobe test: requires CAP_BPF and CAP_PERFMON"]
    fn process_only_and_detaches() {
        // Never start the fixture from a root test harness.
        assert_nonroot(&fs::read_to_string("/proc/self/status").unwrap());
        let descriptors = || fs::read_dir("/proc/self/fd").unwrap().count();
        let baseline = descriptors();
        for _ in 0..2 {
            let Assessment::Observed(sensor) = SelfFsyncSensor::start() else {
                panic!("self-fsync eBPF sensor unavailable");
            };
            assert_eq!(sensor.count(), Assessment::Observed(0));
            sync_own_fixture();
            assert_eq!(sensor.count(), Assessment::Observed(CALLS));

            let child = Command::new("/usr/bin/setpriv")
                .env_clear()
                .args(["--no-new-privs", "--inh-caps=-all", "--ambient-caps=-all"])
                .arg(env::current_exe().unwrap())
                .args(["--ignored", "--exact", "linux::unprivileged_child"])
                .output()
                .unwrap();
            assert!(
                child.status.success(),
                "unprivileged fixture failed: {child:?}"
            );
            // libtest also exits zero on an unmatched --exact filter.
            assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"));
            assert_eq!(sensor.count(), Assessment::Observed(CALLS));

            thread::spawn(sync_own_fixture).join().unwrap();
            assert_eq!(sensor.count(), Assessment::Observed(2 * CALLS));
            drop(sensor);
            assert_eq!(descriptors(), baseline, "link/map/program fd leak");
        }
    }
}
