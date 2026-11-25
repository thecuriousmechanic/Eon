//! Sample unchanged scenes once a second and changed scenes at most five times a second.

use std::collections::VecDeque;

use crate::{EonError, EonResult, VideoKeyframe};

/// A row-major luminance thumbnail from the capture pipeline, independent of render resolution.
pub type Luma = [u8; 32 * 32];
const RETENTION_MS: u64 = 30_000;
const DEFAULT_BYTES: usize = 30 * 1024 * 1024;
const MIN_INTERVAL_MS: u64 = 200;
const HEARTBEAT_MS: u64 = 1_000;

/// Mean structural similarity over sixteen 8x8 blocks. C1 and C2 are the
/// standard stabilizers for 8-bit luminance, including uniform black frames.
fn similarity(a: &Luma, b: &Luma) -> f64 {
    let mut total = 0.0;
    for row in (0..32).step_by(8) {
        for col in (0..32).step_by(8) {
            let (mut x, mut y, mut xx, mut yy, mut xy) = (0., 0., 0., 0., 0.);
            for r in row..row + 8 {
                for c in col..col + 8 {
                    let (p, q) = (f64::from(a[r * 32 + c]), f64::from(b[r * 32 + c]));
                    x += p;
                    y += q;
                    xx += p * p;
                    yy += q * q;
                    xy += p * q;
                }
            }
            let (mx, my) = (x / 64., y / 64.);
            let (vx, vy, cov) = (xx / 64. - mx * mx, yy / 64. - my * my, xy / 64. - mx * my);
            total += ((2. * mx * my + 6.5025) * (2. * cov + 58.5225))
                / ((mx * mx + my * my + 6.5025) * (vx + vy + 58.5225));
        }
    }
    total / 16.
}

/// A byte-bounded history driven by caller-supplied, nondecreasing timestamps.
/// Suitable for offline capture and replay: time advances only on `observe` or `recent`.
pub struct KeyframeHistory<Jpeg> {
    frames: VecDeque<(VideoKeyframe<Jpeg>, usize)>,
    last_luma: Option<Luma>,
    clock_ms: u64,
    sequence: u64,
    bytes: usize,
    budget: usize,
    retention_ms: u64,
}

/// Portable in-process history. Sampling does not grant capture permission.
pub type VisualMemory = KeyframeHistory<Vec<u8>>;

impl<Jpeg> Default for KeyframeHistory<Jpeg> {
    fn default() -> Self {
        Self::with_budget(DEFAULT_BYTES)
    }
}

impl<Jpeg> KeyframeHistory<Jpeg> {
    pub fn with_budget(bytes: usize) -> Self {
        Self::with_limits(bytes, RETENTION_MS)
    }

    pub(crate) fn with_limits(budget: usize, retention_ms: u64) -> Self {
        Self {
            frames: VecDeque::new(),
            last_luma: None,
            clock_ms: 0,
            sequence: 0,
            bytes: 0,
            budget,
            retention_ms,
        }
    }

    fn evict_oldest(&mut self) {
        let (_, len) = self.frames.pop_front().expect("only evict existing frames");
        self.bytes -= len;
    }

    pub(crate) fn retained(&self) -> impl Iterator<Item = &VideoKeyframe<Jpeg>> {
        self.frames.iter().map(|(frame, _)| frame)
    }

    pub(crate) fn next_expiry_ms(&self) -> Option<u64> {
        self.retained()
            .next()?
            .timestamp_ms
            .checked_add(self.retention_ms)
    }

    pub(crate) fn advance(&mut self, now_ms: u64) -> EonResult<()> {
        if now_ms < self.clock_ms {
            return Err(EonError::ValidationError(
                "capture clock moved backwards".into(),
            ));
        }
        self.clock_ms = now_ms;
        while self.next_expiry_ms().is_some_and(|end| end <= now_ms) {
            self.evict_oldest();
        }
        if self.frames.is_empty() {
            self.last_luma = None;
        }
        Ok(())
    }

    pub(crate) fn observe_with(
        &mut self,
        now_ms: u64,
        dimensions: (u32, u32),
        thumbnail: &Luma,
        jpeg: &[u8],
        store: impl FnOnce(&[u8]) -> EonResult<Jpeg>,
    ) -> EonResult<Option<u64>> {
        if dimensions.0 == 0 || dimensions.1 == 0 || jpeg.is_empty() {
            return Err(EonError::ValidationError("empty capture frame".into()));
        }
        self.advance(now_ms)?;
        let mut transition = false;
        if let Some((last, _)) = self.frames.back() {
            let elapsed = now_ms - last.timestamp_ms;
            if elapsed < MIN_INTERVAL_MS {
                return Ok(None);
            }
            transition = (last.width, last.height) != dimensions
                || self
                    .last_luma
                    .as_ref()
                    .is_some_and(|prev| similarity(prev, thumbnail) < 0.95);
            if !transition && elapsed < HEARTBEAT_MS {
                return Ok(None);
            }
        }
        if jpeg.len() > self.budget {
            return Err(EonError::ResourceExhausted {
                resource: "visual memory bytes".into(),
                requested: jpeg.len() as u64,
                available: self.budget as u64,
            });
        }
        let seq = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| EonError::InternalError("keyframe sequence exhausted".into()))?;
        // A storage failure must not evict live frames or advance the sampling state.
        let payload = store(jpeg)?;
        while self.bytes > self.budget - jpeg.len() {
            self.evict_oldest();
        }
        self.frames.push_back((
            VideoKeyframe {
                sequence_number: seq,
                timestamp_ms: now_ms,
                jpeg_bytes: payload,
                width: dimensions.0,
                height: dimensions.1,
                is_scene_transition: transition,
            },
            jpeg.len(),
        ));
        self.last_luma = Some(*thumbnail);
        self.sequence = seq;
        self.bytes += jpeg.len();
        Ok(Some(seq))
    }

    /// Returns the retained part of `[since_ms, now_ms]` in capture order without cloning frames.
    pub fn recent(
        &mut self,
        since_ms: u64,
        now_ms: u64,
    ) -> EonResult<impl Iterator<Item = &VideoKeyframe<Jpeg>>> {
        if since_ms > now_ms {
            return Err(EonError::ValidationError(
                "invalid keyframe time range".into(),
            ));
        }
        self.advance(now_ms)?;
        Ok(self.retained().filter(move |f| f.timestamp_ms >= since_ms))
    }
}

impl VisualMemory {
    /// Copies the caller's compressed bytes only when the sampler accepts a frame.
    pub fn observe(
        &mut self,
        now_ms: u64,
        dimensions: (u32, u32),
        thumbnail: &Luma,
        jpeg: &[u8],
    ) -> EonResult<Option<u64>> {
        self.observe_with(
            now_ms,
            dimensions,
            thumbnail,
            jpeg,
            |data| Ok(data.to_vec()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_failure_leaves_the_previous_sample_and_sequence_intact() {
        let mut ring = VisualMemory::with_budget(1);
        ring.observe(0, (1, 1), &[0; 1024], b"a").unwrap();
        let err = EonError::OperationFailed {
            operation: "test storage".into(),
            reason: "unavailable".into(),
        };
        assert_eq!(
            ring.observe_with(200, (1, 1), &[255; 1024], b"b", |_| Err(err.clone())),
            Err(err)
        );
        assert_eq!(
            ring.recent(0, 200).unwrap().next().unwrap().jpeg_bytes,
            b"a"
        );
        assert_eq!(
            ring.observe(200, (1, 1), &[255; 1024], b"b").unwrap(),
            Some(2)
        );
    }

    #[test]
    fn vision_tests_static_heartbeat_and_dynamic_rate_limit() {
        let (dark, bright) = ([0; 1024], [255; 1024]);
        let mut memory = VisualMemory::default();
        for (t, luma, expected) in [
            (0, &dark, Some(1)),
            (199, &bright, None),
            (200, &bright, Some(2)),
            (399, &dark, None),
            (400, &dark, Some(3)),
            (1399, &dark, None),
            (1400, &dark, Some(4)),
        ] {
            assert_eq!(
                memory.observe(t, (32, 32), luma, b"frame").unwrap(),
                expected
            );
        }
        let transitions: Vec<_> = memory
            .recent(0, 1400)
            .unwrap()
            .map(|f| f.is_scene_transition)
            .collect();
        assert_eq!(transitions, [false, true, true, false]);
    }

    #[test]
    fn vision_tests_local_change_and_geometry_can_trigger_a_sample() {
        let mut m = VisualMemory::default();
        let mut frame = [0; 1024];
        m.observe(0, (128, 128), &frame, b"a").unwrap();
        for row in frame.chunks_mut(32).take(8) {
            row[..8].fill(200);
        }
        assert_eq!(m.observe(200, (128, 128), &frame, b"b").unwrap(), Some(2));
        assert_eq!(m.observe(400, (64, 128), &frame, b"c").unwrap(), Some(3));
    }

    #[test]
    fn vision_tests_evict_by_bytes_or_time_even_without_new_input() {
        let mut m = VisualMemory::with_budget(4);
        for (t, jpeg) in [(0, b"aa"), (1000, b"bb"), (2000, b"cc")] {
            m.observe(t, (1, 1), &[0; 1024], jpeg).unwrap();
        }
        assert!(m.observe(3000, (1, 1), &[0; 1024], b"large").is_err());
        let kept: Vec<_> = m
            .recent(0, 3000)
            .unwrap()
            .map(|f| f.jpeg_bytes.clone())
            .collect();
        assert_eq!(kept, [b"bb", b"cc"]);
        assert!(m.recent(0, 2999).is_err());
        assert_eq!(m.recent(0, 31_000).unwrap().count(), 1);
        assert_eq!(m.recent(0, 32_000).unwrap().count(), 0);
        assert_eq!(
            m.observe(32_000, (1, 1), &[0; 1024], b"d").unwrap(),
            Some(4)
        );
    }
}
