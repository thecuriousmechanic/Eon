use super::windows::CANDIDATE_SAMPLES;
use super::{SpeakerEmbedder, SpeakerEmbedding, SpeakerEncoder, SpeakerGate};
use eon_primitives::{EonError, EonResult};

// Match enrollment duration to inference duration. Bound offline work on long
// authorized recordings and weight excerpts equally, not by their loudness.
fn enroll_windows(
    pcm: &[f32],
    encoder: SpeakerEncoder,
    mut embed: impl FnMut(&[f32]) -> EonResult<SpeakerEmbedding>,
    threshold: f32,
) -> EonResult<SpeakerGate> {
    let n = (pcm.len() / CANDIDATE_SAMPLES).min(8);
    if n == 0 || pcm.iter().any(|x| !x.is_finite()) {
        return Err(EonError::ValidationError(
            "expected at least one full finite speaker window".into(),
        ));
    }
    let span = pcm.len() - CANDIDATE_SAMPLES;
    let mut sum = vec![0.; encoder.dimension()];
    for i in 0..n {
        // For one window take the middle, otherwise include both ends.
        let start = if n == 1 {
            span / 2
        } else {
            (span as u128 * i as u128 / (n - 1) as u128) as usize
        };
        let window = &pcm[start..start + CANDIDATE_SAMPLES];
        if window.iter().all(|&x| x == 0.) {
            continue;
        }
        for (s, x) in sum.iter_mut().zip(embed(window)?.as_slice()) {
            *s += x;
        }
    }
    SpeakerGate::enroll(&SpeakerEmbedding::from_values(encoder, &sum)?, threshold)
}

/// A reply's local speaker policy. Agreement is opt-in and requires a new
/// enrollment. Neither encoder can authorize a mute on the other's behalf.
pub struct SpeakerPolicy {
    primary: SpeakerGate,
    confirmation: Option<(SpeakerGate, SpeakerEmbedder)>,
}

impl From<SpeakerGate> for SpeakerPolicy {
    fn from(primary: SpeakerGate) -> Self {
        Self {
            primary,
            confirmation: None,
        }
    }
}

/// Reusable local enrollment; stores only two model-bound templates, no PCM.
pub struct SpeakerAgreement {
    primary: SpeakerGate,
    confirmation: SpeakerGate,
}

impl SpeakerAgreement {
    pub fn enroll(
        primary: &mut SpeakerEmbedder,
        confirmation: &mut SpeakerEmbedder,
        enrollment: &[f32],
        threshold: f32,
    ) -> EonResult<Self> {
        if primary.encoder() == confirmation.encoder() {
            return Err(EonError::ValidationError(
                "agreement requires distinct speaker encoders".into(),
            ));
        }
        let primary = enroll_windows(enrollment, primary.encoder(), |pcm| primary.embed(pcm), threshold)?;
        let second = enroll_windows(
            enrollment,
            confirmation.encoder(),
            |pcm| confirmation.embed(pcm),
            threshold,
        )?;
        Ok(Self {
            primary,
            confirmation: second,
        })
    }

    /// Bind a reply to both templates. The coordinator checks the primary
    /// encoder before capture; the confirmation encoder is checked here.
    pub fn policy(&self, model: SpeakerEmbedder) -> EonResult<SpeakerPolicy> {
        if model.encoder() != self.confirmation.encoder() {
            return Err(EonError::ValidationError(
                "incorrect speaker confirmation encoder".into(),
            ));
        }
        Ok(SpeakerPolicy {
            primary: self.primary.clone(),
            confirmation: Some((self.confirmation.clone(), model)),
        })
    }
}

impl SpeakerPolicy {
    pub(crate) fn encoder(&self) -> SpeakerEncoder {
        self.primary.encoder()
    }

    pub(crate) fn verify(&mut self, candidate: &SpeakerEmbedding, pcm: &[f32]) -> EonResult<bool> {
        match &mut self.confirmation {
            Some((gate, model)) => agree(self.primary.accepts(candidate), || Ok(gate.accepts(&model.embed(pcm)?))),
            None => Ok(self.primary.accepts(candidate)),
        }
    }
}

fn agree(primary: bool, confirm: impl FnOnce() -> EonResult<bool>) -> EonResult<bool> {
    if primary { confirm() } else { Ok(false) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_single_vote_or_confirmation_error_can_authorize() {
        assert!(!agree(false, || panic!("should short circuit")).unwrap());
        assert!(!agree(true, || Ok(false)).unwrap());
        assert!(agree(true, || Err(EonError::ValidationError("inference failed".into()))).is_err());
        assert!(agree(true, || Ok(true)).unwrap());
    }

    #[test]
    fn enrollment_averages_normalized_current_length_excerpts_not_best_match() {
        let encoder = SpeakerEncoder::WeSpeakerResNet34LmV1;
        let mut basis = vec![0.; encoder.dimension()];
        let pcm: Vec<_> = (0..2 * CANDIDATE_SAMPLES + 1).map(|i| i as f32).collect();
        let mut starts = Vec::new();
        let gate = enroll_windows(
            &pcm,
            encoder,
            |w| {
                assert_eq!(w.len(), CANDIDATE_SAMPLES);
                let i = starts.len();
                starts.push(w[0] as usize);
                basis.fill(0.);
                basis[i] = 1.;
                SpeakerEmbedding::from_values(encoder, &basis)
            },
            0.8,
        )
        .unwrap();
        assert_eq!(starts, [0, CANDIDATE_SAMPLES + 1]);
        assert!(!gate.accepts(&SpeakerEmbedding::from_values(encoder, &basis).unwrap()));
        basis[0] = 1.;
        assert!(gate.accepts(&SpeakerEmbedding::from_values(encoder, &basis).unwrap()));
    }

    #[test]
    fn no_enrollment_from_short_silent_or_invalid_audio() {
        for pcm in [
            vec![1.; CANDIDATE_SAMPLES - 1],
            vec![0.; CANDIDATE_SAMPLES],
            vec![f32::NAN; CANDIDATE_SAMPLES],
        ] {
            assert!(
                enroll_windows(
                    &pcm,
                    SpeakerEncoder::WeSpeakerResNet34LmV1,
                    |_| panic!("invalid enrollment reached model"),
                    0.35
                )
                .is_err()
            );
        }
    }
}
