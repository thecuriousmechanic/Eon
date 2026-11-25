//! Deadlock-free two-phase resource acquisition (docs/implementation/01, section 4.1).
//!
//! Requirements are merged and checked in one pass, in canonical key order. A request either
//! takes every resource it needs or takes nothing and queues, so hold-and-wait never happens and
//! circular wait is impossible. The queue is strict priority: a waiting request is never bypassed
//! by a lower-priority request, which prevents starvation of large requests.
//!
//! There is no background timer. Call [`ResourceManager::reap_expired`] with [`Instant::now`]
//! periodically (even when idle), and dispatch every returned [`Grant`] just as for `release`.
//! To process timeouts before other work, reap first, then acquire or release at the same instant.
//! Use nondecreasing instants from the same monotonic clock for all explicit-time calls.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use eon_primitives::{EonError, EonResult, LeaseId};
use serde::{Deserialize, Serialize};

/// Lifetime of a lease obtained via [`ResourceManager::acquire`], measured from its grant.
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ResourceRank {
    CpuCores = 0,
    GpuCompute = 1,
    MemoryMb = 2,
    NetworkBandwidth = 3,
    FileLock = 4,
}

/// Total order over resources. Every request is sorted by this key before acquisition.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CanonicalResourceKey {
    pub rank: ResourceRank,
    /// Empty for countable pools; a canonical URI for file locks.
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Demand {
    /// Takes `units` from a countable pool.
    Pool(u64),
    /// Shared read access to a lock.
    SharedRead,
    /// Exclusive write access to a lock.
    ExclusiveWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    pub key: CanonicalResourceKey,
    pub demand: Demand,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Acquisition {
    Granted(LeaseId),
    /// Nothing was taken. The caller is identified by `seq` and receives a `Grant` later.
    Queued,
}

/// A queued request that was granted by a later release or timeout sweep.
#[derive(Debug, PartialEq, Eq)]
pub struct Grant {
    pub seq: u64,
    pub lease: LeaseId,
}

#[derive(Debug, Default)]
struct LockState {
    readers: u32,
    writer: bool,
}

#[derive(Debug)]
struct Request {
    requirements: Vec<Requirement>,
    ttl: Duration,
}

#[derive(Debug)]
struct Held {
    request: Request,
    granted_at: Instant,
}

impl Held {
    fn expired_at(&self, now: Instant) -> bool {
        // Compare elapsed time instead of adding a potentially unrepresentable TTL to an Instant.
        now.saturating_duration_since(self.granted_at) >= self.request.ttl
    }

    fn restore(
        &self,
        in_use: &mut BTreeMap<CanonicalResourceKey, u64>,
        locks: &mut BTreeMap<CanonicalResourceKey, LockState>,
    ) {
        for req in &self.request.requirements {
            match req.demand {
                Demand::Pool(units) => *in_use.get_mut(&req.key).expect("pool was held") -= units,
                Demand::SharedRead => locks.get_mut(&req.key).expect("lock was held").readers -= 1,
                Demand::ExclusiveWrite => locks.get_mut(&req.key).expect("lock was held").writer = false,
            }
        }
    }
}

#[derive(Default)]
pub struct ResourceManager {
    capacity: BTreeMap<CanonicalResourceKey, u64>,
    in_use: BTreeMap<CanonicalResourceKey, u64>,
    locks: BTreeMap<CanonicalResourceKey, LockState>,
    leases: BTreeMap<LeaseId, Held>,
    /// Waiters ordered by `(priority, seq)`, lower values first.
    waiting: BTreeMap<(u32, u64), Request>,
    /// A Grant identifies its waiter by seq alone, regardless of priority.
    waiting_seqs: BTreeSet<u64>,
}

impl ResourceManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_capacity(&mut self, key: CanonicalResourceKey, units: u64) {
        self.capacity.insert(key, units);
    }

    /// Acquires everything or queues with [`DEFAULT_LEASE_TTL`]. `priority` is lower-first;
    /// `seq` breaks ties in arrival order and must be unique among waiting requests.
    ///
    /// Errors when a pool demand exceeds total capacity, since such a request could never be granted
    /// and would block every request queued behind it.
    pub fn acquire(&mut self, priority: u32, seq: u64, requirements: Vec<Requirement>) -> EonResult<Acquisition> {
        self.acquire_at(priority, seq, requirements, DEFAULT_LEASE_TTL, Instant::now())
    }

    /// Like [`Self::acquire`], with an explicit grant time and a positive TTL. If queued, the
    /// TTL starts at the release or reap that grants the request, not at `now`.
    pub fn acquire_at(
        &mut self,
        priority: u32,
        seq: u64,
        requirements: Vec<Requirement>,
        ttl: Duration,
        now: Instant,
    ) -> EonResult<Acquisition> {
        if ttl.is_zero() {
            return Err(EonError::ValidationError("lease TTL must be positive".into()));
        }
        if self.waiting_seqs.contains(&seq) {
            return Err(EonError::ValidationError(format!("request {seq} is already queued")));
        }
        let merged = merge(requirements)?;
        self.check_capacity(&merged)?;

        let key = (priority, seq);
        let request = Request { requirements: merged, ttl };
        let ahead_in_queue = self.waiting.range(..key).next().is_some();
        if !ahead_in_queue && self.satisfiable(&request.requirements) {
            return Ok(Acquisition::Granted(self.take(request, now)));
        }
        self.waiting.insert(key, request);
        self.waiting_seqs.insert(seq);
        Ok(Acquisition::Queued)
    }

    pub(crate) fn check_capacity(&self, merged: &[Requirement]) -> EonResult<()> {
        for req in merged {
            if let Demand::Pool(units) = req.demand {
                let cap = self.capacity.get(&req.key).copied().unwrap_or(0);
                if units > cap {
                    return Err(EonError::ResourceExhausted {
                        resource: format!("{:?}:{}", req.key.rank, req.key.id),
                        requested: units,
                        available: cap,
                    });
                }
            }
        }

        Ok(())
    }

    /// Releases a lease, then grants queued requests in priority order until the first one that does not fit.
    pub fn release(&mut self, lease: &LeaseId) -> EonResult<Vec<Grant>> {
        self.release_at(lease, Instant::now())
    }

    /// Like [`Self::release`], at an explicit instant. An unknown or expired lease returns
    /// `ResourceLeaseTimeout` without changing state; only [`Self::reap_expired`] reclaims timeouts.
    pub fn release_at(&mut self, lease: &LeaseId, now: Instant) -> EonResult<Vec<Grant>> {
        let held = self.leases.get(lease)
            .filter(|held| !held.expired_at(now))
            .ok_or_else(|| EonError::ResourceLeaseTimeout {
                lease_id: lease.to_string(),
            })?;
        held.restore(&mut self.in_use, &mut self.locks);
        self.leases.remove(lease);
        Ok(self.grant_in_order(now))
    }

    /// Reclaims all leases whose TTL has elapsed (including the exact boundary), then grants
    /// waiters once in priority order. Expired resources remain reserved until this is called.
    #[must_use = "dispatch each grant to its waiter; it will not be returned again"]
    pub fn reap_expired(&mut self, now: Instant) -> Vec<Grant> {
        self.leases.retain(|_, held| {
            if held.expired_at(now) {
                held.restore(&mut self.in_use, &mut self.locks);
                false
            } else {
                true
            }
        });
        self.grant_in_order(now)
    }

    pub fn available(&self, key: &CanonicalResourceKey) -> u64 {
        let cap = self.capacity.get(key).copied().unwrap_or(0);
        // Capacity may have been lowered below outstanding holdings.
        cap.saturating_sub(self.in_use.get(key).copied().unwrap_or(0))
    }

    pub fn waiting_len(&self) -> usize {
        self.waiting.len()
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.waiting.is_empty() && self.leases.is_empty()
    }

    /// A scheduler finishes one result at a time and hands out *all* newly available grants
    /// on its next dispatch tick. No grant (or TTL) can be lost between these calls.
    pub(crate) fn relinquish(&mut self, lease: &LeaseId) {
        let held = self.leases.remove(lease).expect("scheduler owns the lease");
        held.restore(&mut self.in_use, &mut self.locks);
    }

    pub(crate) fn revoke_all(&mut self) {
        self.leases.clear();
        self.in_use.clear();
        self.locks.clear();
        self.waiting.clear();
        self.waiting_seqs.clear();
    }

    fn satisfiable(&self, reqs: &[Requirement]) -> bool {
        reqs.iter().all(|req| match req.demand {
            Demand::Pool(units) => self.available(&req.key) >= units,
            Demand::SharedRead => !self.locks.get(&req.key).is_some_and(|s| s.writer || s.readers == u32::MAX),
            Demand::ExclusiveWrite => !self
                .locks
                .get(&req.key)
                .is_some_and(|s| s.writer || s.readers > 0),
        })
    }

    fn take(&mut self, request: Request, now: Instant) -> LeaseId {
        let id = LeaseId::new();
        for req in &request.requirements {
            match req.demand {
                Demand::Pool(units) => {
                    *self.in_use.entry(req.key.clone()).or_insert(0) += units;
                }
                Demand::SharedRead => {
                    self.locks.entry(req.key.clone()).or_default().readers += 1;
                }
                Demand::ExclusiveWrite => {
                    self.locks.entry(req.key.clone()).or_default().writer = true;
                }
            }
        }
        self.leases.insert(id.clone(), Held { request, granted_at: now });
        id
    }

    /// Head-of-line grant: stop at the first waiter that does not fit, so no lower priority request overtakes it.
    fn grant_in_order(&mut self, now: Instant) -> Vec<Grant> {
        let mut granted = Vec::new();
        while let Some((&key, request)) = self.waiting.first_key_value() {
            if !self.satisfiable(&request.requirements) {
                break;
            }
            let request = self.waiting.remove(&key).expect("key was just observed");
            self.waiting_seqs.remove(&key.1);
            granted.push(Grant {
                seq: key.1,
                lease: self.take(request, now),
            });
        }
        granted
    }
}

/// Collapses duplicate keys. Pools sum. Lock demands upgrade to the strongest mode.
/// Mixing a pool and a lock on one key is a programming error.
pub(crate) fn merge(requirements: Vec<Requirement>) -> EonResult<Vec<Requirement>> {
    let mut by_key: BTreeMap<CanonicalResourceKey, Demand> = BTreeMap::new();
    for req in requirements {
        match (req.key.rank, req.demand) {
            (ResourceRank::FileLock, Demand::SharedRead) => {
                by_key.entry(req.key).or_insert(Demand::SharedRead);
            }
            (ResourceRank::FileLock, Demand::ExclusiveWrite) => {
                by_key.insert(req.key, Demand::ExclusiveWrite);
            }
            (rank, Demand::Pool(units)) if rank != ResourceRank::FileLock => {
                let total = match by_key.get(&req.key) {
                    Some(Demand::Pool(previous)) => previous.checked_add(units).ok_or_else(|| {
                        EonError::ValidationError(format!("pool demand overflow on {:?}", req.key))
                    })?,
                    _ => units,
                };
                by_key.insert(req.key, Demand::Pool(total));
            }
            _ => {
                return Err(EonError::ValidationError(format!(
                    "invalid demand {:?} for {:?}", req.demand, req.key
                )));
            }
        }
    }
    Ok(by_key
        .into_iter()
        .map(|(key, demand)| Requirement { key, demand })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(rank: ResourceRank, id: &str, units: u64) -> Requirement {
        Requirement {
            key: CanonicalResourceKey { rank, id: id.into() },
            demand: Demand::Pool(units),
        }
    }

    fn lock(id: &str, demand: Demand) -> Requirement {
        Requirement {
            key: CanonicalResourceKey {
                rank: ResourceRank::FileLock,
                id: id.into(),
            },
            demand,
        }
    }

    fn cpu_key() -> CanonicalResourceKey {
        CanonicalResourceKey {
            rank: ResourceRank::CpuCores,
            id: String::new(),
        }
    }

    fn granted(acq: Acquisition) -> LeaseId {
        match acq {
            Acquisition::Granted(lease) => lease,
            Acquisition::Queued => panic!("expected a grant"),
        }
    }

    #[test]
    fn partial_request_takes_nothing_when_one_resource_is_short() {
        let mut rm = ResourceManager::new();
        rm.set_capacity(cpu_key(), 4);
        rm.set_capacity(CanonicalResourceKey { rank: ResourceRank::GpuCompute, id: String::new() }, 1);
        granted(rm.acquire(0, 1, vec![pool(ResourceRank::GpuCompute, "", 1)]).unwrap());

        let greedy = rm
            .acquire(1, 2, vec![pool(ResourceRank::CpuCores, "", 2), pool(ResourceRank::GpuCompute, "", 1)])
            .unwrap();
        assert_eq!(greedy, Acquisition::Queued);
        assert_eq!(rm.available(&cpu_key()), 4, "a queued request must not hold partial CPU");
    }

    #[test]
    fn opposite_order_requests_cannot_deadlock() {
        let mut rm = ResourceManager::new();
        for id in ["/a", "/b"] {
            rm.set_capacity(CanonicalResourceKey { rank: ResourceRank::FileLock, id: id.into() }, 1);
        }
        granted(rm.acquire(0, 1, vec![lock("/b", Demand::ExclusiveWrite), lock("/a", Demand::ExclusiveWrite)]).unwrap());
        let second = rm
            .acquire(0, 2, vec![lock("/a", Demand::ExclusiveWrite), lock("/b", Demand::ExclusiveWrite)])
            .unwrap();
        assert_eq!(second, Acquisition::Queued);
    }

    #[test]
    fn release_grants_highest_priority_waiter_with_its_seq() {
        let mut rm = ResourceManager::new();
        rm.set_capacity(cpu_key(), 2);
        let holder = granted(rm.acquire(0, 1, vec![pool(ResourceRank::CpuCores, "", 2)]).unwrap());
        assert_eq!(rm.acquire(5, 2, vec![pool(ResourceRank::CpuCores, "", 2)]).unwrap(), Acquisition::Queued);
        assert_eq!(rm.acquire(1, 3, vec![pool(ResourceRank::CpuCores, "", 2)]).unwrap(), Acquisition::Queued);

        let grants = rm.release(&holder).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].seq, 3, "priority 1 must beat priority 5");
        assert_eq!(rm.waiting_len(), 1);
    }

    #[test]
    fn large_high_priority_request_is_not_starved_by_small_later_ones() {
        let mut rm = ResourceManager::new();
        rm.set_capacity(cpu_key(), 4);
        let holder = granted(rm.acquire(0, 1, vec![pool(ResourceRank::CpuCores, "", 3)]).unwrap());
        assert_eq!(rm.acquire(0, 2, vec![pool(ResourceRank::CpuCores, "", 4)]).unwrap(), Acquisition::Queued);
        // One CPU is free, so this would fit now, but a higher-priority waiter is ahead of it.
        assert_eq!(rm.acquire(9, 3, vec![pool(ResourceRank::CpuCores, "", 1)]).unwrap(), Acquisition::Queued);

        let grants = rm.release(&holder).unwrap();
        assert_eq!(grants.iter().map(|g| g.seq).collect::<Vec<_>>(), vec![2], "the 4-CPU request goes first");
    }

    #[test]
    fn request_above_capacity_is_refused_instead_of_queued_forever() {
        let mut rm = ResourceManager::new();
        rm.set_capacity(cpu_key(), 2);
        let err = rm.acquire(0, 1, vec![pool(ResourceRank::CpuCores, "", 3)]).unwrap_err();
        assert!(matches!(err, EonError::ResourceExhausted { requested: 3, .. }));
        assert_eq!(rm.waiting_len(), 0);
    }

    #[test]
    fn shared_readers_coexist_but_writer_waits() {
        let mut rm = ResourceManager::new();
        let r1 = granted(rm.acquire(0, 1, vec![lock("/doc", Demand::SharedRead)]).unwrap());
        granted(rm.acquire(0, 2, vec![lock("/doc", Demand::SharedRead)]).unwrap());
        assert_eq!(rm.acquire(0, 3, vec![lock("/doc", Demand::ExclusiveWrite)]).unwrap(), Acquisition::Queued);
        assert!(rm.release(&r1).unwrap().is_empty(), "one reader still holds the lock");
    }

    #[test]
    fn duplicate_pool_requests_are_summed_before_checking() {
        let mut rm = ResourceManager::new();
        rm.set_capacity(cpu_key(), 3);
        let err = rm
            .acquire(0, 1, vec![pool(ResourceRank::CpuCores, "", 2), pool(ResourceRank::CpuCores, "", 2)])
            .unwrap_err();
        assert!(matches!(err, EonError::ResourceExhausted { requested: 4, .. }), "2+2 must be summed, then refused");
    }

    #[test]
    fn malformed_requests_neither_reserve_nor_queue() {
        let mut rm = ResourceManager::new();
        rm.set_capacity(cpu_key(), u64::MAX);
        let cpu = |units| pool(ResourceRank::CpuCores, "", units);
        for reqs in [
            vec![cpu(u64::MAX), cpu(1)],
            vec![lock("/a", Demand::Pool(1)), lock("/a", Demand::ExclusiveWrite)],
            vec![lock("/a", Demand::ExclusiveWrite), lock("/a", Demand::Pool(1))],
            vec![Requirement { key: cpu_key(), demand: Demand::SharedRead }],
        ] {
            assert!(matches!(rm.acquire(0, 1, reqs), Err(EonError::ValidationError(_))));
        }
        assert!(matches!(
            rm.acquire_at(0, 1, vec![cpu(1)], Duration::ZERO, Instant::now()),
            Err(EonError::ValidationError(_))
        ));
        assert_eq!(rm.available(&cpu_key()), u64::MAX);
        assert_eq!(rm.waiting_len(), 0);
    }

    #[test]
    fn a_retried_waiter_cannot_replace_or_bypass_its_original_request() {
        let mut rm = ResourceManager::new();
        let cpu = |units| vec![pool(ResourceRank::CpuCores, "", units)];
        rm.set_capacity(cpu_key(), 2);
        let holder = granted(rm.acquire(0, 1, cpu(1)).unwrap());
        assert_eq!(rm.acquire(3, 2, cpu(2)).unwrap(), Acquisition::Queued);
        for priority in [3, 0, 9] {
            assert!(matches!(rm.acquire(priority, 2, cpu(1)), Err(EonError::ValidationError(_))));
        }
        assert_eq!(rm.available(&cpu_key()), 1);
        assert_eq!(rm.waiting_len(), 1);
        let grants = rm.release(&holder).unwrap();
        assert_eq!(grants.iter().map(|g| g.seq).collect::<Vec<_>>(), [2]);
        assert_eq!(rm.available(&cpu_key()), 0);
    }

    #[test]
    fn extreme_ttl_and_a_capacity_shrink_do_not_wrap() {
        let mut rm = ResourceManager::new();
        let now = Instant::now();
        rm.set_capacity(cpu_key(), u64::MAX);
        let holder = granted(rm.acquire_at(
            0, 1, vec![pool(ResourceRank::CpuCores, "", u64::MAX)], Duration::MAX, now,
        ).unwrap());
        rm.set_capacity(cpu_key(), 1);
        assert_eq!(rm.available(&cpu_key()), 0);
        assert!(rm.reap_expired(now).is_empty());
        assert!(rm.release_at(&holder, now).unwrap().is_empty());
        assert_eq!(rm.available(&cpu_key()), 1);
    }

    #[test]
    fn releasing_unknown_lease_is_an_error() {
        let mut rm = ResourceManager::new();
        assert!(matches!(rm.release(&LeaseId::new()), Err(EonError::ResourceLeaseTimeout { .. })));
    }
}
