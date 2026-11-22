//! Hybrid memory ranking, procedural decay, and memory governance (docs/implementation/02, sections 4 to 5).
//!
//! Reciprocal rank fusion merges keyword, vector, and graph rankings without tuning score scales.
//! A Wilson lower bound ranks procedures by evidence, so a macro that worked twice does not outrank one
//! that worked 900 times out of 1000. The governor decides which memory tiers stay resident when the
//! system goes dormant.

use std::collections::BTreeMap;

mod appraisal;
pub use appraisal::{
    Provenance, ReferenceKind, TaskDiagnostic, UntrustedNote, WorkingEntry, WorkingForm, WorkingLedger,
};

#[cfg(feature = "bge-memory")]
mod bge;
#[cfg(feature = "bge-memory")]
pub use bge::BgeMicroV2;
#[cfg(feature = "sqlite-memory")]
mod graph;
#[cfg(feature = "sqlite-memory")]
mod hybrid;
#[cfg(feature = "sqlite-memory")]
pub use hybrid::{AppraisalMemory, Embedding, EmbeddingSpace, RecallHit};
#[cfg(feature = "loro-memory")]
pub use hybrid::PairedGraphMemory;

pub const RRF_K: f64 = 60.0;

/// Fuses ranked lists. `rankings[i]` is ordered best first. Ties break on the id for stable output.
pub fn reciprocal_rank_fusion(rankings: &[Vec<String>]) -> Vec<(String, f64)> {
    let mut scores: BTreeMap<&str, f64> = BTreeMap::new();
    for ranking in rankings {
        for (index, id) in ranking.iter().enumerate() {
            *scores.entry(id.as_str()).or_insert(0.0) += 1.0 / (RRF_K + (index + 1) as f64);
        }
    }
    let mut fused: Vec<(String, f64)> = scores.into_iter().map(|(id, s)| (id.to_string(), s)).collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    fused
}

/// Lower bound of the Wilson score interval for a success rate. Zero trials gives zero.
pub fn wilson_lower_bound(successes: u64, trials: u64, z: f64) -> f64 {
    if trials == 0 {
        return 0.0;
    }
    let n = trials as f64;
    let p = successes.min(trials) as f64 / n;
    let z2 = z * z;
    let centre = p + z2 / (2.0 * n);
    let margin = z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt();
    ((centre - margin) / (1.0 + z2 / n)).max(0.0)
}

pub const Z_95: f64 = 1.96;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemoryTier {
    /// Tier 1: preferences and identity. Always resident.
    Identity,
    /// Tier 2: semantic and procedural indexes. Dropped when dormant.
    Index,
    /// Tier 3: model weights. Released with madvise(MADV_DONTNEED) when dormant.
    Weights,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Active,
    Dormant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub mode: Mode,
    /// Tiers to release on this transition. Empty when no release is needed.
    pub release: Vec<MemoryTier>,
}

#[derive(Debug)]
pub struct MemoryGovernor {
    mode: Mode,
}

impl Default for MemoryGovernor {
    fn default() -> Self {
        Self { mode: Mode::Active }
    }
}

impl MemoryGovernor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Entering dormant releases tiers 2 and 3. Identity (tier 1) is never released.
    /// Re-entering active reloads lazily, so no tiers are released on that edge.
    pub fn set_mode(&mut self, next: Mode) -> Transition {
        let release = match (self.mode, next) {
            (Mode::Active, Mode::Dormant) => vec![MemoryTier::Index, MemoryTier::Weights],
            _ => Vec::new(),
        };
        self.mode = next;
        Transition { mode: next, release }
    }

    pub fn is_resident(&self, tier: MemoryTier) -> bool {
        match (self.mode, tier) {
            (_, MemoryTier::Identity) => true,
            (Mode::Active, _) => true,
            (Mode::Dormant, _) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rrf_rewards_agreement_across_rankings() {
        let keyword = ids(&["a", "b", "c"]);
        let vector = ids(&["b", "a", "d"]);
        let fused = reciprocal_rank_fusion(&[keyword, vector]);
        let order: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(order[0], "a", "a and b tie on score; id breaks the tie");
        assert_eq!(order[1], "b");
        assert!(order.contains(&"d"));
    }

    #[test]
    fn rrf_is_deterministic_and_bounded_by_list_length() {
        let fused = reciprocal_rank_fusion(&[ids(&["x"]), ids(&["y"])]);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused, reciprocal_rank_fusion(&[ids(&["x"]), ids(&["y"])]));
    }

    #[test]
    fn wilson_penalises_small_samples() {
        let tiny = wilson_lower_bound(2, 2, Z_95);
        let large = wilson_lower_bound(900, 1000, Z_95);
        assert!(large > tiny, "900/1000 must outrank 2/2 ({large} vs {tiny})");
        assert!(large > 0.87 && large < 0.92, "900/1000 lower bound is about 0.88, got {large}");
    }

    #[test]
    fn wilson_handles_zero_trials_and_stays_in_unit_range() {
        assert_eq!(wilson_lower_bound(0, 0, Z_95), 0.0);
        for (s, n) in [(0u64, 10u64), (5, 10), (10, 10), (1, 1_000_000)] {
            let v = wilson_lower_bound(s, n, Z_95);
            assert!((0.0..=1.0).contains(&v), "{s}/{n} gave {v}");
        }
    }

    #[test]
    fn dormant_releases_index_and_weights_but_never_identity() {
        let mut gov = MemoryGovernor::new();
        let t = gov.set_mode(Mode::Dormant);
        assert_eq!(t.release, vec![MemoryTier::Index, MemoryTier::Weights]);
        assert!(gov.is_resident(MemoryTier::Identity));
        assert!(!gov.is_resident(MemoryTier::Weights));
    }

    #[test]
    fn waking_reloads_lazily_and_repeating_dormant_releases_nothing_new() {
        let mut gov = MemoryGovernor::new();
        gov.set_mode(Mode::Dormant);
        assert!(gov.set_mode(Mode::Dormant).release.is_empty());
        assert!(gov.set_mode(Mode::Active).release.is_empty());
        assert!(gov.is_resident(MemoryTier::Weights));
    }
}
