//! Model-based tests for the resource manager (docs/implementation/01, section 4.1).
//!
//! Random sequences of acquire and release operations run against both the real manager and a simple
//! model. After every step the test checks capacity, exclusive-lock safety, and that the grant set matches
//! the model. At the end every outstanding lease is released and the queue must drain completely.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use eon_kernel::resource::{
    Acquisition, CanonicalResourceKey, Demand, Grant, Requirement, ResourceManager, ResourceRank,
};
use eon_primitives::{EonError, LeaseId};
use proptest::prelude::*;

const CPU_CAPACITY: u64 = 4;
const FILES: [&str; 3] = ["/src/a.rs", "/src/b.rs", "/src/c.rs"];

#[derive(Debug, Clone)]
enum Op {
    Acquire { priority: u32, cpu: u64, file: usize, write: bool },
    Release { index: usize },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0u32..4, 1u64..=CPU_CAPACITY, 0usize..FILES.len(), any::<bool>())
            .prop_map(|(priority, cpu, file, write)| Op::Acquire { priority, cpu, file, write }),
        2 => any::<prop::sample::Index>().prop_map(|index| Op::Release { index: index.index(usize::MAX) }),
    ]
}

fn cpu_key() -> CanonicalResourceKey {
    CanonicalResourceKey { rank: ResourceRank::CpuCores, id: String::new() }
}

fn file_key(file: usize) -> CanonicalResourceKey {
    CanonicalResourceKey { rank: ResourceRank::FileLock, id: FILES[file].into() }
}

fn requirements(cpu: u64, file: usize, write: bool) -> Vec<Requirement> {
    vec![
        Requirement { key: cpu_key(), demand: Demand::Pool(cpu) },
        Requirement {
            key: file_key(file),
            demand: if write { Demand::ExclusiveWrite } else { Demand::SharedRead },
        },
    ]
}

#[test]
fn reap_grants_in_order_once_and_stale_releases_cannot_reclaim_the_replacements() {
    let mut rm = ResourceManager::new();
    rm.set_capacity(cpu_key(), 4);
    let start = Instant::now();
    let at = |seconds| start + Duration::from_secs(seconds);
    let mut acquire = |priority, seq, cpu, file, write, seconds| {
        rm.acquire_at(priority, seq, requirements(cpu, file, write), Duration::from_secs(seconds), start).unwrap()
    };
    // Both readers' pools and locks must be returned before the head waiter can run.
    let readers = [1, 2].map(|seq| match acquire(0, seq, 2, 0, false, 10) {
        Acquisition::Granted(lease) => lease,
        Acquisition::Queued => panic!("readers should coexist"),
    });
    assert_eq!(acquire(9, 3, 3, 0, true, 1), Acquisition::Queued);
    assert_eq!(acquire(1, 4, 3, 0, true, 3), Acquisition::Queued);
    assert_eq!(acquire(2, 5, 1, 1, true, 9), Acquisition::Queued);
    assert!(rm.reap_expired(at(9)).is_empty());
    assert_eq!(rm.available(&cpu_key()), 0);

    // Even an overdue release before the sweep must not silently consume a grant.
    assert!(matches!(rm.release_at(&readers[0], at(10)), Err(EonError::ResourceLeaseTimeout { .. })));
    let grants = rm.reap_expired(at(10));
    assert_eq!(grants.iter().map(|g| g.seq).collect::<Vec<_>>(), [4, 5]);
    for old in &readers {
        assert!(matches!(rm.release_at(old, at(10)), Err(EonError::ResourceLeaseTimeout { .. })));
    }
    assert!(rm.reap_expired(at(10)).is_empty());
    assert!(rm.reap_expired(at(12)).is_empty());
    assert_eq!(rm.available(&cpu_key()), 0);
    assert_eq!(rm.waiting_len(), 1);

    // The writer's three seconds started at grant, despite waiting ten seconds in the queue.
    let last = rm.reap_expired(at(13));
    assert_eq!(last.iter().map(|g| g.seq).collect::<Vec<_>>(), [3]);
    assert!(matches!(rm.release_at(&grants[0].lease, at(13)), Err(EonError::ResourceLeaseTimeout { .. })));
    assert!(rm.release_at(&grants[1].lease, at(13)).unwrap().is_empty());
    assert_eq!(rm.available(&cpu_key()), 1);
    assert!(rm.reap_expired(at(14)).is_empty());
    assert_eq!(rm.available(&cpu_key()), 4);
    assert_eq!(rm.waiting_len(), 0);
}

#[test]
fn release_starts_a_waiters_ttl_only_after_the_last_reader_is_gone() {
    let mut rm = ResourceManager::new();
    rm.set_capacity(cpu_key(), 2);
    let start = Instant::now();
    let ttl = Duration::from_secs;
    let mut acquire = |seq, reqs, seconds| rm.acquire_at(0, seq, reqs, ttl(seconds), start).unwrap();
    assert!(matches!(acquire(1, requirements(1, 0, false), 2), Acquisition::Granted(_)));
    let Acquisition::Granted(reader) = acquire(2, requirements(1, 0, false), 10) else {
        panic!("second reader should be granted");
    };
    let mut writer = requirements(1, 0, true);
    writer.push(Requirement { key: file_key(0), demand: Demand::SharedRead });
    assert_eq!(acquire(3, writer, 2), Acquisition::Queued);
    assert_eq!(acquire(4, requirements(1, 0, false), 5), Acquisition::Queued);

    // One CPU is now free, but the writer still conflicts and the later reader cannot bypass it.
    assert!(rm.reap_expired(start + ttl(2)).is_empty());
    assert_eq!(rm.available(&cpu_key()), 1);
    let grants = rm.release_at(&reader, start + ttl(9)).unwrap();
    assert_eq!(grants.iter().map(|g| g.seq).collect::<Vec<_>>(), [3]);
    assert!(rm.reap_expired(start + ttl(10)).is_empty());
    let grants = rm.reap_expired(start + ttl(11));
    assert_eq!(grants.iter().map(|g| g.seq).collect::<Vec<_>>(), [4]);
    assert!(rm.release_at(&grants[0].lease, start + ttl(11)).unwrap().is_empty());
    assert_eq!(rm.available(&cpu_key()), 2);
}

/// What the model knows about each granted lease.
#[derive(Clone)]
struct ModelLease {
    cpu: u64,
    file: usize,
    write: bool,
}

#[derive(Default)]
struct Model {
    held: BTreeMap<u64, ModelLease>,
    lease_of_seq: BTreeMap<u64, LeaseId>,
    queued: BTreeSet<u64>,
}

impl Model {
    fn check(&self, rm: &ResourceManager) -> Result<(), TestCaseError> {
        let cpu_used: u64 = self.held.values().map(|l| l.cpu).sum();
        prop_assert!(cpu_used <= CPU_CAPACITY, "CPU over-committed: {cpu_used}");
        prop_assert_eq!(rm.available(&cpu_key()), CPU_CAPACITY - cpu_used);

        for (file, _) in FILES.iter().enumerate() {
            let writers = self.held.values().filter(|l| l.file == file && l.write).count();
            let readers = self.held.values().filter(|l| l.file == file && !l.write).count();
            prop_assert!(writers <= 1, "two writers on {}", FILES[file]);
            prop_assert!(writers == 0 || readers == 0, "writer coexists with readers on {}", FILES[file]);
        }
        prop_assert_eq!(rm.waiting_len(), self.queued.len());
        Ok(())
    }

    fn apply_grants(&mut self, grants: Vec<Grant>, waiting: &mut BTreeMap<u64, (u64, usize, bool)>) -> Result<(), TestCaseError> {
        for grant in grants {
            prop_assert!(self.queued.remove(&grant.seq), "granted seq {} was not queued", grant.seq);
            let (cpu, file, write) = waiting.remove(&grant.seq).expect("queued request is tracked");
            self.held.insert(grant.seq, ModelLease { cpu, file, write });
            self.lease_of_seq.insert(grant.seq, grant.lease);
        }
        Ok(())
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn random_operations_respect_capacity_and_exclusivity_and_always_drain(
        ops in prop::collection::vec(op_strategy(), 1..80),
    ) {
        let mut rm = ResourceManager::new();
        rm.set_capacity(cpu_key(), CPU_CAPACITY);
        for file in 0..FILES.len() {
            rm.set_capacity(file_key(file), 1);
        }

        let mut model = Model::default();
        let mut pending_requests: BTreeMap<u64, (u64, usize, bool)> = BTreeMap::new();
        let mut next_seq = 1u64;

        for op in ops {
            match op {
                Op::Acquire { priority, cpu, file, write } => {
                    let seq = next_seq;
                    next_seq += 1;
                    match rm.acquire(priority, seq, requirements(cpu, file, write)).unwrap() {
                        Acquisition::Granted(lease) => {
                            model.held.insert(seq, ModelLease { cpu, file, write });
                            model.lease_of_seq.insert(seq, lease);
                        }
                        Acquisition::Queued => {
                            model.queued.insert(seq);
                            pending_requests.insert(seq, (cpu, file, write));
                        }
                    }
                }
                Op::Release { index } => {
                    let keys: Vec<u64> = model.held.keys().copied().collect();
                    if keys.is_empty() {
                        continue;
                    }
                    let seq = keys[index % keys.len()];
                    let lease = model.lease_of_seq.remove(&seq).unwrap();
                    model.held.remove(&seq);
                    let grants = rm.release(&lease).unwrap();
                    model.apply_grants(grants, &mut pending_requests)?;
                }
            }
            model.check(&rm)?;
        }

        // Liveness: releasing every held lease must eventually grant every queued request.
        while let Some(&seq) = model.held.keys().next() {
            let lease = model.lease_of_seq.remove(&seq).unwrap();
            model.held.remove(&seq);
            let grants = rm.release(&lease).unwrap();
            model.apply_grants(grants, &mut pending_requests)?;
            model.check(&rm)?;
        }
        prop_assert!(model.queued.is_empty(), "requests never drained: {:?}", model.queued);
        prop_assert_eq!(rm.waiting_len(), 0);
        prop_assert_eq!(rm.available(&cpu_key()), CPU_CAPACITY);
    }
}
