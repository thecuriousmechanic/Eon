//! Early-exit intent classification for a *replaceable* ASR hypothesis.
//! Labels are local routing previews, not endpoints, credentials or commands.

use crate::endpoint::ReflexHint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentPreview {
    Reflex(ReflexHint),
    Question,
    Request,
    /// A multi-word utterance without a recognized command or query opening.
    OpenSpeech,
}

impl IntentPreview {
    /// Inspect at most the first 128 UTF-8 bytes and three words. Anchor on the
    /// opening, never a keyword somewhere in quoted/negated speech. Re-evaluate
    /// the current hypothesis, not a growing history of partials.
    pub fn from_partial(text: &str) -> Option<Self> {
        const LIMIT: usize = 128;
        if text.len() <= LIMIT
            && let Some(hint) = ReflexHint::from_partial(text)
        {
            return Some(Self::Reflex(hint));
        }
        let prefix = if text.len() <= LIMIT {
            text
        } else {
            // A word split by the inspection budget isn't evidence.
            let prefix = &text[..text.floor_char_boundary(LIMIT)];
            prefix.rsplit_once(char::is_whitespace)?.0
        };
        let mut words = prefix.split_ascii_whitespace();
        let (first, second) = (words.next()?, words.next()?);
        let starts = |options: &[&str]| options.iter().any(|w| first.eq_ignore_ascii_case(w));
        if starts(&["please"]) || (starts(&["could", "would", "can", "will"]) && second.eq_ignore_ascii_case("you")) {
            return Some(Self::Request);
        }
        let auxiliary = starts(&[
            "is", "are", "was", "were", "do", "does", "did", "can", "could", "will", "would",
        ]);
        let subject = [
            "i", "it", "we", "you", "they", "he", "she", "this", "that", "there", "the",
        ]
        .iter()
        .any(|w| second.eq_ignore_ascii_case(w));
        if starts(&["what", "why", "where", "when", "who", "how", "which"]) || (auxiliary && subject) {
            return Some(Self::Question);
        }
        words.next().map(|_| Self::OpenSpeech)
    }
}

#[cfg(test)]
mod asr_tests {
    use super::*;

    #[test]
    fn classify_the_current_hypothesis_without_waiting_for_eof() {
        use IntentPreview::*;
        assert_eq!(IntentPreview::from_partial("HOW"), None);
        assert_eq!(IntentPreview::from_partial("HOW LONG"), Some(Question));
        assert_eq!(IntentPreview::from_partial("will it take"), Some(Question));
        assert_eq!(IntentPreview::from_partial("could you"), Some(Request));
        assert_eq!(
            IntentPreview::from_partial("RUN TESTS"),
            Some(Reflex(ReflexHint::RunTests))
        );
        assert_eq!(IntentPreview::from_partial("RUN TESTS AND"), Some(OpenSpeech));
        assert_eq!(IntentPreview::from_partial("do not stop"), Some(OpenSpeech));
        assert_eq!(IntentPreview::from_partial("STO"), None);
        assert_eq!(IntentPreview::from_partial(""), None);
    }

    #[test]
    fn never_classify_a_truncated_prefix_as_a_closed_command() {
        let text = format!("stop{}later", " ".repeat(512));
        assert_eq!(IntentPreview::from_partial(&text), None);
        assert_eq!(
            IntentPreview::from_partial(&format!("would{}yourself", " ".repeat(120))),
            None
        );
        assert_eq!(IntentPreview::from_partial(&"界".repeat(100)), None);
    }
}
