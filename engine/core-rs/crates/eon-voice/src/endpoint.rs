//! Three-tier endpointing (docs/implementation/04, section 3.3).
//!
//! The end-of-turn silence window depends on whether the transcript so far reads as a finished thought.
//! A 250 ms cutoff chops multi-clause commands (median clause pause is about 388 ms), so an unfinished
//! sentence waits longer. A 300 ms holdoff after speech resumes stops false endpoints on breath noise.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    /// The transcript ends a sentence, for example after a terminal word or a question.
    Complete,
    /// The transcript could continue or stop.
    Ambiguous,
    /// The transcript clearly expects more, for example after a conjunction or an unfinished clause.
    Incomplete,
}

/// Speculative intent only; a partial can be retracted and never authorizes an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReflexHint {
    Stop,
    Affirmative,
    RunTests,
    QuietMode,
}

impl ReflexHint {
    pub fn from_partial(text: &str) -> Option<Self> {
        let text = text.trim().trim_end_matches(['.', '?', '!', ',', ';', ':']).trim_end();
        [
            ("stop", Self::Stop),
            ("affirmative", Self::Affirmative),
            ("run tests", Self::RunTests),
            ("quiet mode", Self::QuietMode),
        ]
        .into_iter()
        .find_map(|(command, hint)| text.eq_ignore_ascii_case(command).then_some(hint))
    }
}

impl Completeness {
    /// Conservative tier selection for a live Zipformer partial. An incidental
    /// command inside a longer (possibly negated) sentence is not a fast reflex.
    pub fn from_partial(text: &str) -> Self {
        let text = text.trim().trim_end_matches(['.', '?', '!', ',', ';', ':']).trim_end();
        if text.split_ascii_whitespace().next_back().is_some_and(|last| {
            ["and", "or", "but", "in", "to", "for", "with", "then", "because", "if"]
                .iter()
                .any(|w| last.eq_ignore_ascii_case(w))
        }) {
            return Self::Incomplete;
        }
        if ReflexHint::from_partial(text).is_some() {
            Self::Complete
        } else {
            Self::Ambiguous
        }
    }
}

pub const TIER_COMPLETE_MS: u32 = 300;
pub const TIER_AMBIGUOUS_MS: u32 = 800;
pub const TIER_INCOMPLETE_MS: u32 = 1150;
/// Silence shorter than this after speech resumes is treated as a breath, not a new pause.
pub const RESUME_HOLDOFF_MS: u32 = 300;

pub fn silence_threshold_ms(completeness: Completeness) -> u32 {
    match completeness {
        Completeness::Complete => TIER_COMPLETE_MS,
        Completeness::Ambiguous => TIER_AMBIGUOUS_MS,
        Completeness::Incomplete => TIER_INCOMPLETE_MS,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub silence_ms: u32,
}

#[derive(Debug, Default)]
pub struct Endpointer {
    speaking: bool,
    silence_ms: u32,
}

impl Endpointer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one VAD frame. Returns an endpoint when the silence reaches the tier for `completeness`.
    pub fn push_frame(&mut self, frame_ms: u32, is_speech: bool, completeness: Completeness) -> Option<Endpoint> {
        if is_speech {
            self.speaking = true;
            self.silence_ms = 0;
            return None;
        }
        if !self.speaking {
            return None;
        }
        self.silence_ms = self.silence_ms.saturating_add(frame_ms);
        if self.silence_ms >= silence_threshold_ms(completeness) {
            let endpoint = Endpoint {
                silence_ms: self.silence_ms,
            };
            self.speaking = false;
            self.silence_ms = 0;
            return Some(endpoint);
        }
        None
    }

    /// Whether a silence gap has ended with speech resuming inside the holdoff, meaning it was a breath.
    pub fn is_breath(silence_ms: u32) -> bool {
        silence_ms < RESUME_HOLDOFF_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_hint_can_be_retracted_and_does_not_match_an_embedded_command() {
        assert_eq!(ReflexHint::from_partial("RUN TESTS."), Some(ReflexHint::RunTests));
        assert_eq!(Completeness::from_partial("RUN TESTS AND"), Completeness::Incomplete);
        assert_eq!(ReflexHint::from_partial("run tests and"), None);
        assert_eq!(Completeness::from_partial("do not run tests"), Completeness::Ambiguous);
    }

    /// Feeds `speech_frames` of speech, then silence of `silence_ms`, and reports whether an endpoint fired.
    fn endpoints_after(silence_ms: u32, completeness: Completeness) -> bool {
        let mut ep = Endpointer::new();
        ep.push_frame(32, true, completeness);
        let mut fired = false;
        for _ in 0..(silence_ms / 32) {
            fired |= ep.push_frame(32, false, completeness).is_some();
        }
        fired
    }

    #[test]
    fn complete_transcript_endpoints_at_300_ms() {
        assert!(!endpoints_after(256, Completeness::Complete));
        assert!(endpoints_after(320, Completeness::Complete));
    }

    #[test]
    fn incomplete_transcript_waits_for_1150_ms_and_survives_a_clause_pause() {
        assert!(
            !endpoints_after(388, Completeness::Incomplete),
            "median clause pause must not end the turn"
        );
        assert!(!endpoints_after(800, Completeness::Incomplete));
        assert!(endpoints_after(1184, Completeness::Incomplete));
    }

    #[test]
    fn ambiguous_transcript_uses_the_middle_tier() {
        assert!(!endpoints_after(768, Completeness::Ambiguous));
        assert!(endpoints_after(832, Completeness::Ambiguous));
    }

    #[test]
    fn resumed_speech_resets_the_silence_clock() {
        let mut ep = Endpointer::new();
        ep.push_frame(32, true, Completeness::Complete);
        for _ in 0..8 {
            ep.push_frame(32, false, Completeness::Complete);
        }
        ep.push_frame(32, true, Completeness::Complete);
        let mut fired = false;
        for _ in 0..8 {
            fired |= ep.push_frame(32, false, Completeness::Complete).is_some();
        }
        assert!(!fired, "256 ms of silence split by speech must not endpoint");
    }

    #[test]
    fn leading_silence_before_any_speech_never_endpoints() {
        let mut ep = Endpointer::new();
        for _ in 0..100 {
            assert!(ep.push_frame(32, false, Completeness::Complete).is_none());
        }
    }

    #[test]
    fn breath_classification_uses_the_holdoff() {
        assert!(Endpointer::is_breath(RESUME_HOLDOFF_MS - 1));
        assert!(!Endpointer::is_breath(RESUME_HOLDOFF_MS));
    }
}
