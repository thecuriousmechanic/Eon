//! Normalised LMS echo canceller for barge-in detection (docs/implementation/04, echo path).
//!
//! The barge-in detector needs to hear the owner over the assistant's own echo. AEC3's suppressor removes near-end
//! speech while the render is active, so its output cannot be used to detect the owner (see the loopback test). This
//! canceller models only the linear echo from the render, so uncorrelated speech passes through unchanged. It is not a
//! substitute for AEC3 on the ASR path.

pub struct NlmsCanceller {
    weights: Vec<f32>,
    history: Vec<f32>, // mirrored ring: history[head..head+taps] runs newest to oldest
    head: usize,
    power: f64,
    mu: f32,
}

const POWER_FLOOR: f32 = 1e-6;

impl NlmsCanceller {
    /// `taps` must cover the echo delay plus its tail. `mu` is the step size, in (0, 2).
    pub fn new(taps: usize, mu: f32) -> Self {
        Self {
            weights: vec![0.0; taps],
            history: vec![0.0; 2 * taps],
            head: 0,
            power: 0.0,
            mu,
        }
    }

    /// After skipping device PCM, the old delay line cannot predict echo at the new
    /// position. Keep the learned acoustic path, not its samples.
    #[cfg(feature = "native")]
    pub(crate) fn discard_render_history(&mut self) {
        self.history.fill(0.0);
        self.power = 0.0;
    }

    /// Adds one playback sample to the reference history.
    pub fn push_render(&mut self, sample: f32) {
        let taps = self.weights.len();
        self.head = if self.head == 0 {
            taps - 1
        } else {
            self.head - 1
        };
        let oldest = self.history[self.head];
        // Avoid accumulated f32 roundoff masquerading as render power after a fade.
        self.power = (self.power - f64::from(oldest).powi(2)).max(0.0) + f64::from(sample).powi(2);
        self.history[self.head] = sample;
        self.history[self.head + taps] = sample;
    }

    /// Returns the microphone sample minus the predicted echo, and adapts the weights.
    pub fn process(&mut self, mic: f32) -> f32 {
        let taps = self.weights.len();
        let reference = &self.history[self.head..self.head + taps];
        let predicted = self
            .weights
            .iter()
            .zip(reference)
            .fold(0.0, |sum, (w, x)| sum + w * x);
        let residual = mic - predicted;
        // Plain NLMS divides by render power even in a pause. A loud near-end
        // phoneme over a fading reference can then corrupt the echo path for
        // seconds. Reduce the step when the unexplained mic energy dominates.
        let gain = self.mu * residual
            / (self.power as f32 + taps as f32 * residual * residual + POWER_FLOOR);
        for (weight, &sample) in self.weights.iter_mut().zip(reference) {
            *weight += gain * sample;
        }
        residual
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_zero_is_newest_even_across_non_power_of_two_wraps() {
        for taps in [1, 3, 17] {
            let mut nlms = NlmsCanceller::new(taps, 0.2);
            let mut samples = vec![0.; taps];
            let mut weights = vec![0.; taps];
            let mut power = 0.0f64;
            for i in 0..100 {
                let render = ((i * 13 % 23) as f32 - 11.) / 16.;
                let mic = ((i * 7 % 31) as f32 - 15.) / 16.;
                let oldest = samples.pop().unwrap();
                power = (power - f64::from(oldest).powi(2)).max(0.0) + f64::from(render).powi(2);
                samples.insert(0, render);
                let residual = mic
                    - weights
                        .iter()
                        .zip(&samples)
                        .fold(0., |sum, (w, x)| sum + w * x);
                let gain = 0.2 * residual
                    / (power as f32 + taps as f32 * residual * residual + POWER_FLOOR);
                for (w, x) in weights.iter_mut().zip(&samples) {
                    *w += gain * x;
                }
                nlms.push_render(render);
                assert_eq!(nlms.process(mic).to_bits(), residual.to_bits());
                assert_eq!(nlms.weights, weights);
            }
        }
    }

    #[test]
    fn near_end_during_a_render_fade_does_not_corrupt_the_echo_path() {
        // A broadband path unrelated to any enrolled voice or test recording.
        // Train it, keep another source talking through a quiet far end, then
        // measure the *first* block after render resumes, not after retraining.
        let mut state = 17u32;
        let render: Vec<_> = (0..8200)
            .map(|i| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                let amplitude = if (4000..8000).contains(&i) { 0.0003 } else { 0.3 };
                (state as f32 / u32::MAX as f32 - 0.5) * amplitude
            })
            .collect();
        let mut nlms = NlmsCanceller::new(128, 0.2);
        let (mut error, mut echo_energy) = (0.0, 0.0);
        for (i, &sample) in render.iter().enumerate() {
            nlms.push_render(sample);
            let echo = if i >= 91 { 0.6 * render[i - 37] - 0.2 * render[i - 91] } else { 0.0 };
            let near = if (4000..8000).contains(&i) { 0.2 * (0.08 * i as f32).sin() } else { 0.0 };
            let residual = nlms.process(echo + near);
            if i >= 8000 {
                error += residual * residual;
                echo_energy += echo * echo;
            }
        }
        assert!(error < 0.1 * echo_energy, "lost the echo path during double talk: {error}/{echo_energy}");
    }

    #[test]
    fn echo_is_removed_and_uncorrelated_speech_survives() {
        let rate = 16_000.0f32;
        let delay = 720;
        let total = 48_000;
        let owner_start = 32_000;
        let mut canceller = NlmsCanceller::new(1024, 0.2);
        let render: Vec<f32> = (0..total)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate).sin() * 0.4)
            .collect();
        let mut lcg = 12345u64;
        let owner: Vec<f32> = (0..total)
            .map(|i| {
                lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                if i < owner_start {
                    0.0
                } else {
                    ((lcg >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.2
                }
            })
            .collect();

        let mut echo_only_residual = 0.0f32;
        let mut echo_only_count = 0usize;
        let mut owner_error = 0.0f32;
        let mut owner_energy = 0.0f32;
        for i in 0..total {
            canceller.push_render(render[i]);
            let echo = if i >= delay { render[i - delay] * 0.5 } else { 0.0 };
            let residual = canceller.process(echo + owner[i]);
            if (24_000..owner_start).contains(&i) {
                echo_only_residual += residual * residual;
                echo_only_count += 1;
            }
            if i >= owner_start + 8_000 {
                owner_error += (residual - owner[i]).powi(2);
                owner_energy += owner[i] * owner[i];
            }
        }
        let echo_only_power = echo_only_residual / echo_only_count as f32;
        assert!(echo_only_power < 1e-4, "echo residual power {echo_only_power} after convergence");
        assert!(
            owner_error < owner_energy * 0.2,
            "owner speech distorted: error {owner_error} against energy {owner_energy}"
        );
    }
}
