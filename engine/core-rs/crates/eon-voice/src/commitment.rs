//! Reply-scoped synthesis on the worker thread. Only fixed, cached bridges may
//! be heard before the kernel confirms a lease for this epoch.

use std::collections::VecDeque;
use std::sync::Arc;

use eon_primitives::{EonError, EonResult, LeaseId};

use crate::barge_in::BargeIn;
use crate::device::{PlaybackHandle, play_default_output_with_reference};
use crate::resample::resample_mono;
use crate::ring::{PcmConsumer, PcmProducer, pcm_ring};
use crate::tts::{KokoroTts, MAX_CLAUSE_BYTES, PreparedClause, SAMPLE_RATE_HZ, clauses};

const MAX_QUEUED_CLAUSES: usize = 32;
const MAX_QUEUED_BYTES: usize = 8192;

fn check_rate(hz: u32) -> EonResult<()> {
    if (8_000..=192_000).contains(&hz) {
        Ok(())
    } else {
        Err(EonError::ValidationError("unsupported reply output sample rate".into()))
    }
}

#[derive(Clone, Copy)]
pub enum Bridge {
    Affirmative,
    Report,
    Notice,
    Negative,
    Proposal,
}

pub struct BridgeCache {
    pcm: [Arc<[f32]>; 5],
    rate: u32,
}

/// A silent inference has no usable cache. Model, frontend and conversion
/// failures remain errors and are not evidence of a silent waveform.
pub enum BridgeWarmup {
    Ready(BridgeCache),
    SilentBridge,
}

impl BridgeCache {
    /// Run once during initialization, not on the turn/latency measurement path.
    pub fn warm(tts: &mut KokoroTts, speed: f32) -> EonResult<Self> {
        Self::warm_for_output(tts, speed, SAMPLE_RATE_HZ)
    }

    /// Convert bridges at initialization, never on a prefix request or in a device callback.
    pub fn warm_for_output(tts: &mut KokoroTts, speed: f32, rate: u32) -> EonResult<Self> {
        match Self::measure_warmup(tts, speed, rate)? {
            BridgeWarmup::Ready(cache) => Ok(cache),
            BridgeWarmup::SilentBridge => Err(EonError::ValidationError("Kokoro returned a silent bridge".into())),
        }
    }

    pub fn measure_warmup(tts: &mut KokoroTts, speed: f32, rate: u32) -> EonResult<BridgeWarmup> {
        check_rate(rate)?;
        let mut pcm = std::array::from_fn(|_| Arc::default());
        for (samples, text) in pcm
            .iter_mut()
            .zip(["Affirmative.", "Report:", "Notice:", "Negative.", "Proposal:"])
        {
            // synthesize validates nonempty, finite model output before this check.
            let Some(wave) = prepare_bridge(tts.synthesize(text, speed)?) else {
                return Ok(BridgeWarmup::SilentBridge);
            };
            *samples = device_pcm(wave, rate)?.into();
        }
        Ok(BridgeWarmup::Ready(Self { pcm, rate }))
    }
}

fn device_pcm(samples: Vec<f32>, rate: u32) -> EonResult<Vec<f32>> {
    let mut samples = if rate == SAMPLE_RATE_HZ {
        samples
    } else {
        resample_mono(&samples, SAMPLE_RATE_HZ, rate)?
    };
    // Bandlimited interpolation can overshoot near full scale.
    samples.iter_mut().for_each(|s| *s = s.clamp(-1.0, 1.0));
    Ok(samples)
}

// Kokoro can pad a short prefix with hundreds of milliseconds of near-silence. Strip only
// the leading pad, retaining 20 ms before the first -60 dBFS sample for the initial consonant.
fn prepare_bridge(mut pcm: Vec<f32>) -> Option<Vec<f32>> {
    let onset = pcm.iter().position(|s| s.abs() >= 0.001)?;
    let keep_from = onset.saturating_sub(SAMPLE_RATE_HZ as usize / 50);
    pcm.drain(..keep_from);
    let fade = (15 * SAMPLE_RATE_HZ as usize / 1000).min(pcm.len());
    for (i, sample) in pcm.iter_mut().rev().take(fade).enumerate() {
        *sample *= 0.5 - 0.5 * (std::f32::consts::PI * i as f32 / fade as f32).cos();
    }
    Some(pcm)
}

pub struct CommitmentGate {
    epoch: u64,
    lease: Option<LeaseId>,
    pending: VecDeque<Clause>,
    playback: BargeIn,
}

enum Clause {
    Text(String),
    Prepared(PreparedClause),
}

impl Clause {
    fn text(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Prepared(clause) => clause.text(),
        }
    }

    fn synthesize(&self, tts: &mut KokoroTts, speed: f32, playback: &BargeIn) -> EonResult<Vec<f32>> {
        match self {
            Self::Text(text) => tts.synthesize_cancellable(text, speed, || playback.is_muted()),
            Self::Prepared(clause) => tts.synthesize_prepared(clause, speed),
        }
    }
}

impl CommitmentGate {
    pub fn new(epoch: u64, playback: BargeIn) -> Self {
        Self {
            epoch,
            lease: None,
            pending: VecDeque::new(),
            playback,
        }
    }

    pub fn bridge<'a>(&self, cache: &'a BridgeCache, bridge: Bridge) -> Option<&'a [f32]> {
        if self.playback.is_muted() {
            return None;
        }
        Some(&cache.pcm[bridge as usize])
    }

    /// Free text is always a claim, even when it happens to start like a bridge.
    pub fn queue(&mut self, text: &str) -> EonResult<()> {
        self.check_capacity(text.len(), || clauses(text).take(MAX_QUEUED_CLAUSES + 1).count())?;
        if clauses(text).any(|s| s.len() > MAX_CLAUSE_BYTES) {
            return Err(EonError::ValidationError("reply clause too long".into()));
        }
        self.pending.extend(clauses(text).map(|s| Clause::Text(s.to_owned())));
        Ok(())
    }

    fn check_capacity(&self, extra_bytes: usize, extra_clauses: impl FnOnce() -> usize) -> EonResult<()> {
        if self.playback.is_muted() {
            return Err(EonError::ValidationError("reply was cancelled".into()));
        }
        let bytes: usize = self.pending.iter().map(|c| c.text().len()).sum();
        if extra_bytes > MAX_QUEUED_BYTES - bytes || extra_clauses() > MAX_QUEUED_CLAUSES - self.pending.len() {
            return Err(EonError::ValidationError(
                "reply text queue full; no clauses accepted".into(),
            ));
        }
        Ok(())
    }

    /// Called by the owner on a trusted kernel event, never from a model's text.
    pub fn lease_acquired(&mut self, epoch: u64, lease: LeaseId) {
        if epoch == self.epoch && self.lease.is_none() && !self.playback.is_muted() {
            self.lease = Some(lease);
        }
    }

    pub fn lease(&self) -> Option<&LeaseId> {
        self.lease.as_ref().filter(|_| !self.playback.is_muted())
    }

    /// Once the render callback may have received PCM, invalidation must mute the whole
    /// reply. A "Negative." / "Notice:" pivot belongs on a fresh reply, not behind stale claims.
    pub fn invalidate(&mut self, epoch: u64) {
        if epoch == self.epoch {
            self.playback.trigger();
            self.lease = None;
            self.pending.clear();
        }
    }

    fn ready(&self) -> Option<&Clause> {
        self.lease()?;
        self.pending.front()
    }

    /// Software-only path; driver callers should use `ReplyRenderer` which also fences already queued PCM.
    pub fn synthesize_next(&mut self, tts: &mut KokoroTts, speed: f32) -> EonResult<Option<Vec<f32>>> {
        let Some(text) = self.ready() else { return Ok(None) };
        let samples = match text.synthesize(tts, speed, &self.playback) {
            Ok(samples) => Some(samples),
            Err(_) if self.playback.is_muted() => None,
            Err(e) => return Err(e),
        };
        self.pending.pop_front();
        Ok(samples.filter(|_| !self.playback.is_muted()))
    }
}

/// Epoch-scoped control for the trusted kernel thread. Cancels the *same* ring
/// the callback reads even while the worker is in an ONNX call or backpressured.
#[derive(Clone)]
pub struct ReplyRevocation {
    epoch: u64,
    playback: BargeIn,
}

impl ReplyRevocation {
    pub fn invalidate(&self, epoch: u64) {
        if self.epoch == epoch {
            self.playback.trigger();
        }
    }
}

/// One reply, one producer, at most one clause of unsent PCM. All synthesis and resampling
/// run on the caller's worker; the device callback only reads the existing bounded mono ring.
pub struct ReplyRenderer {
    gate: CommitmentGate,
    tx: PcmProducer,
    rate: u32,
    staged: Option<Arc<[f32]>>,
    offset: usize,
    started: bool,
}

impl ReplyRenderer {
    /// Opens *output only*. `reference` contains exactly what the callback played (including
    /// mute/underrun silence), at `device.sample_rate`, for a local AEC worker.
    pub fn open_default(epoch: u64) -> EonResult<(Self, PlaybackHandle, PcmConsumer)> {
        let signal = BargeIn::new();
        let (tx, rx) = pcm_ring(1 << 15);
        let (reference_tx, reference) = pcm_ring(1 << 15);
        let device = play_default_output_with_reference(rx, signal.clone(), reference_tx)?;
        Ok((Self::new(epoch, signal, tx, device.sample_rate)?, device, reference))
    }

    /// For an existing render endpoint, pass its actual mono producer, negotiated rate
    /// and the **same reply's** driver/barge-in signal. Never reuse a cancelled ring.
    pub fn new(epoch: u64, signal: BargeIn, tx: PcmProducer, rate: u32) -> EonResult<Self> {
        check_rate(rate)?;
        Ok(Self {
            gate: CommitmentGate::new(epoch, signal),
            tx,
            rate,
            staged: None,
            offset: 0,
            started: false,
        })
    }

    pub fn revocation(&self) -> ReplyRevocation {
        ReplyRevocation {
            epoch: self.gate.epoch,
            playback: self.gate.playback.clone(),
        }
    }

    /// Shared with the verified-speaker reflex; also governs the post-render reference.
    pub fn playback_signal(&self) -> BargeIn {
        self.gate.playback.clone()
    }

    /// Queue one or more complete clauses; do not pass partial tokens here.
    pub fn queue(&mut self, text: &str) -> EonResult<()> {
        self.gate.queue(text)
    }

    /// Stage a finalized pronunciation. The same epoch/lease and revocation
    /// checks govern both plain text and prepared model inputs.
    pub fn queue_prepared(&mut self, clause: PreparedClause) -> EonResult<()> {
        self.gate.check_capacity(clause.text().len(), || 1)?;
        if clauses(clause.text()).count() != 1 {
            return Err(EonError::ValidationError("expected one readback clause".into()));
        }
        self.gate.pending.push_back(Clause::Prepared(clause));
        Ok(())
    }

    pub fn lease_acquired(&mut self, epoch: u64, lease: LeaseId) {
        self.gate.lease_acquired(epoch, lease);
    }

    /// Publish the first samples immediately, without touching the model or waiting for a lease.
    pub fn start_bridge(&mut self, cache: &BridgeCache, bridge: Bridge) -> EonResult<usize> {
        if self.started || cache.rate != self.rate || self.gate.bridge(cache, bridge).is_none() {
            return Err(EonError::ValidationError(
                "prefix not permitted or output rate mismatch".into(),
            ));
        }
        self.started = true;
        self.staged = Some(Arc::clone(&cache.pcm[bridge as usize]));
        Ok(self.publish())
    }

    /// Publish a partial ring write, or synthesize and publish at most one committed clause.
    /// Zero means no space, no ready clause, or cancellation; never busy-wait on the callback.
    pub fn pump(&mut self, tts: &mut KokoroTts, speed: f32) -> EonResult<usize> {
        self.pump_with(|clause, playback| clause.synthesize(tts, speed, playback))
    }

    fn pump_with(&mut self, synthesize: impl FnOnce(&Clause, &BargeIn) -> EonResult<Vec<f32>>) -> EonResult<usize> {
        if self.gate.playback.is_muted() || self.staged.is_some() {
            return Ok(self.publish());
        }
        let Some(text) = self.gate.ready() else { return Ok(0) };
        if self.tx.writable() == 0 {
            return Ok(0);
        }
        let samples = synthesize(text, &self.gate.playback);
        if !self.gate.playback.is_muted() {
            self.staged = Some(device_pcm(samples?, self.rate)?.into());
            self.started = true;
        }
        self.gate.pending.pop_front();
        Ok(self.publish())
    }

    fn publish(&mut self) -> usize {
        if self.gate.playback.is_muted() {
            self.gate.pending.clear();
            self.staged = None;
            return 0;
        }
        let Some(samples) = &self.staged else { return 0 };
        let n = self.tx.writable().min(samples.len() - self.offset);
        let written = self.tx.push_slice(&samples[self.offset..self.offset + n]);
        self.offset += written;
        if self.offset == samples.len() {
            self.staged = None;
            self.offset = 0;
        }
        // A concurrent invalidation after the producer check still mutes the callback.
        written
    }
}

#[cfg(test)]
mod commitment_gate_tests {
    use super::*;

    #[test]
    fn prepared_bridges_preserve_a_preroll_and_fade_without_erasing_the_onset() {
        let mut pcm = vec![0.0001; 9000];
        pcm.extend_from_slice(&[0.4; 1600]);
        let prepared = prepare_bridge(pcm).unwrap();
        assert_eq!(prepared.len(), 480 + 1600);
        assert_eq!(&prepared[478..482], &[0.0001, 0.0001, 0.4, 0.4]);
        assert_eq!(*prepared.last().unwrap(), 0.0);
        assert!(prepare_bridge(vec![0.0; 1024]).is_none());
    }

    #[test]
    fn text_queue_limits_are_atomic_and_do_not_evict_earlier_claims() {
        let mut gate = CommitmentGate::new(1, BargeIn::new());
        gate.queue("First.").unwrap();
        assert!(gate.queue(&"a.".repeat(MAX_QUEUED_CLAUSES)).is_err());
        assert!(gate.queue(&"a".repeat(MAX_CLAUSE_BYTES + 1)).is_err());
        assert!(gate.queue(&" ".repeat(MAX_QUEUED_BYTES)).is_err());
        assert_eq!(gate.pending.len(), 1);
        assert_eq!(gate.pending[0].text(), "First.");
    }

    #[test]
    fn prefix_then_committed_claim_survive_partial_writes_without_loss_or_resynthesis() {
        let signal = BargeIn::new();
        let (tx, mut rx) = pcm_ring(2);
        let mut worker = ReplyRenderer::new(9, signal.clone(), tx, SAMPLE_RATE_HZ).unwrap();
        let cache = BridgeCache {
            pcm: std::array::from_fn(|_| Arc::from([0.1, 0.2, 0.3])),
            rate: SAMPLE_RATE_HZ,
        };
        worker.queue("First claim. Second claim.").unwrap();
        assert_eq!(worker.start_bridge(&cache, Bridge::Report).unwrap(), 2);
        assert!(worker.start_bridge(&cache, Bridge::Notice).is_err());
        let no_run = |_: &Clause, _: &BargeIn| panic!("should not start synthesis");
        assert_eq!(worker.pump_with(no_run).unwrap(), 0);
        let mut played = [0.; 2];
        signal.render(&mut rx, &mut played);
        assert_eq!(played, [0.1, 0.2]);
        assert_eq!(worker.pump_with(no_run).unwrap(), 1);
        worker.lease_acquired(8, LeaseId::new());
        assert_eq!(worker.pump_with(no_run).unwrap(), 0);
        worker.lease_acquired(9, LeaseId::new());
        worker.revocation().invalidate(8);
        assert_eq!(
            worker
                .pump_with(|text, _| {
                    assert_eq!(text.text(), "First claim.");
                    Ok(vec![0.4, 0.5, 0.6])
                })
                .unwrap(),
            1
        );
        signal.render(&mut rx, &mut played);
        assert_eq!(played, [0.3, 0.4]);
        assert_eq!(worker.pump_with(no_run).unwrap(), 2);
        assert_eq!(worker.pump_with(no_run).unwrap(), 0);
        assert_eq!(rx.dropped(), 0, "backpressure is not an overrun");
        worker.revocation().invalidate(9);
        let muted = signal.render(&mut rx, &mut played);
        assert_eq!(muted.flushed, 2);
        assert_eq!(played, [0.; 2]);
        assert_eq!(worker.pump_with(no_run).unwrap(), 0);
        assert!(worker.gate.pending.is_empty());
        assert!(worker.queue("Late claim.").is_err());
    }

    #[test]
    fn a_revocation_during_synthesis_prevents_late_pcm_and_leaves_the_ring_muted() {
        let signal = BargeIn::new();
        let (tx, mut rx) = pcm_ring(4);
        let mut worker = ReplyRenderer::new(2, signal.clone(), tx, SAMPLE_RATE_HZ).unwrap();
        let control = worker.revocation();
        worker.queue("Ready? Yes.").unwrap();
        worker.lease_acquired(2, LeaseId::new());
        assert_eq!(
            worker
                .pump_with(|_, playback| {
                    control.invalidate(2);
                    assert!(playback.is_muted());
                    Ok(vec![1.0; 8])
                })
                .unwrap(),
            0
        );
        assert_eq!(rx.available(), 0);
        assert!(worker.gate.pending.is_empty() && worker.staged.is_none());
        assert!(signal.render(&mut rx, &mut [1.; 4]).acknowledged);
    }

    #[test]
    fn a_claim_requires_the_current_lease_and_cannot_survive_invalidation() {
        let mut gate = CommitmentGate::new(4, BargeIn::new());
        gate.queue("Affirmative. I patched it.").unwrap();
        assert!(gate.ready().is_none());
        gate.lease_acquired(3, LeaseId::new());
        assert!(gate.ready().is_none());
        gate.lease_acquired(4, LeaseId::new());
        assert_eq!(gate.ready().map(Clause::text), Some("Affirmative."));
        gate.invalidate(4);
        gate.lease_acquired(4, LeaseId::new());
        assert!(gate.ready().is_none());
        assert!(gate.pending.is_empty());
    }

    #[test]
    fn interrupted_synthesis_cannot_dequeue_another_claim() {
        let signal = BargeIn::new();
        let mut gate = CommitmentGate::new(3, signal.clone());
        gate.queue("Deployed.").unwrap();
        gate.lease_acquired(3, LeaseId::new());
        assert_eq!(gate.ready().map(Clause::text), Some("Deployed."));
        signal.trigger();
        assert!(gate.ready().is_none());
    }
}
