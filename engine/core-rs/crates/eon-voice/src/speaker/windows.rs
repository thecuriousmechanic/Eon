//! Only samples in the current short voiced interval may identify the speaker.

use super::SpeakerEncoder;
use crate::geometry::VAD_FRAME_SAMPLES;
use eon_primitives::EonResult;

const FRAMES: usize = super::MIN_SAMPLES.div_ceil(VAD_FRAME_SAMPLES);
pub const CANDIDATE_SAMPLES: usize = FRAMES * VAD_FRAME_SAMPLES;

pub(crate) struct SpeakerWindows {
    pcm: Vec<f32>,
    phases: usize,
    phase: usize,
}

impl SpeakerWindows {
    pub(crate) fn new(encoder: SpeakerEncoder) -> Self {
        Self {
            pcm: Vec::with_capacity(CANDIDATE_SAMPLES),
            phases: match encoder {
                SpeakerEncoder::WeSpeakerResNet34LmV1 => 2,
                SpeakerEncoder::CamPlusPlusZhEnAdvancedV1 => 1,
            },
            phase: 0,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.pcm.clear();
        self.phase = 0;
    }

    pub(crate) fn candidate<T>(
        &mut self,
        frame: &[f32],
        evaluate: impl FnOnce(&[f32]) -> EonResult<T>,
    ) -> EonResult<Option<T>> {
        // The VAD validated the frame geometry before admission.
        debug_assert_eq!(frame.len(), VAD_FRAME_SAMPLES);
        self.pcm.extend_from_slice(frame);
        if self.pcm.len() < CANDIDATE_SAMPLES {
            return Ok(None);
        }
        debug_assert_eq!(self.pcm.len(), CANDIDATE_SAMPLES);
        let result = evaluate(&self.pcm);
        if result.is_err() {
            self.clear();
        } else {
            // Space probes evenly in whole frames without losing the original
            // disjoint-window endpoints: alternating 8 and 9 frames for WeSpeaker.
            let hop = (self.phase + 1) * FRAMES / self.phases - self.phase * FRAMES / self.phases;
            self.pcm.drain(..hop * VAD_FRAME_SAMPLES);
            self.phase = (self.phase + 1) % self.phases;
        }
        result.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speaker::SpeakerEmbedding;
    use eon_primitives::EonError;

    #[test]
    fn all_probes_are_current_short_intervals_and_preserve_disjoint_endpoints() {
        for (encoder, phases) in [
            (SpeakerEncoder::WeSpeakerResNet34LmV1, 2),
            (SpeakerEncoder::CamPlusPlusZhEnAdvancedV1, 1),
        ] {
            let mut windows = SpeakerWindows::new(encoder);
            let mut probes = Vec::new();
            for i in 1..=3 * FRAMES {
                windows
                    .candidate(&[i as f32; VAD_FRAME_SAMPLES], |pcm| {
                        assert_eq!(pcm.len(), CANDIDATE_SAMPLES);
                        for (offset, frame) in
                            pcm.as_chunks::<VAD_FRAME_SAMPLES>().0.iter().enumerate()
                        {
                            assert!(frame.iter().all(|&x| x == (i + 1 - FRAMES + offset) as f32));
                        }
                        probes.push(i);
                        SpeakerEmbedding::from_values(encoder, &vec![1.; encoder.dimension()])
                    })
                    .unwrap();
            }
            assert_eq!(probes.len(), 2 * phases + 1);
            for endpoint in [FRAMES, 2 * FRAMES, 3 * FRAMES] {
                assert!(probes.contains(&endpoint));
            }
            assert_eq!(windows.pcm.capacity(), CANDIDATE_SAMPLES);
        }
    }

    #[test]
    fn errors_and_gaps_never_splice_voiced_runs() {
        let encoder = SpeakerEncoder::WeSpeakerResNet34LmV1;
        let mut windows = SpeakerWindows::new(encoder);
        for i in 1..=FRAMES {
            let result = windows.candidate(&[1.; VAD_FRAME_SAMPLES], |_| {
                Err::<(), _>(EonError::ValidationError("inference failed".into()))
            });
            assert_eq!(result.is_err(), i == FRAMES);
        }
        for sample in [2., 3.] {
            for _ in 0..FRAMES - 1 {
                assert!(
                    windows
                        .candidate::<()>(&[sample; VAD_FRAME_SAMPLES], |_| panic!("stale samples"))
                        .unwrap()
                        .is_none()
                );
            }
            if sample == 2. {
                windows.clear();
            }
        }
        assert!(
            windows
                .candidate(&[3.; VAD_FRAME_SAMPLES], |pcm| {
                    assert!(pcm.iter().all(|&x| x == 3.));
                    SpeakerEmbedding::from_values(encoder, &vec![1.; encoder.dimension()])
                })
                .unwrap()
                .is_some()
        );
    }
}
