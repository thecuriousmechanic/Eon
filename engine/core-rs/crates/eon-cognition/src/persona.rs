//! Deterministic verbal output for already-public facts, alerts and acknowledgments.

use eon_primitives::{EonError, EonResult};

/// Selected by the caller, not inferred from model sentiment. Formatting is not authorization
/// to act or evidence that an action succeeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpeechAct {
    Affirmative,
    Report,
    Notice,
    Negative,
    Proposal,
}

impl SpeechAct {
    pub(crate) const ALL: [Self; 5] = [
        Self::Affirmative,
        Self::Report,
        Self::Notice,
        Self::Negative,
        Self::Proposal,
    ];

    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Affirmative => "Affirmative.",
            Self::Report => "Report:",
            Self::Notice => "Notice:",
            Self::Negative => "Negative.",
            Self::Proposal => "Proposal:",
        }
    }
}

// Match phrases at word boundaries, not substrings of facts such as "high pressure".
const FILLER: &[&str] = &[
    "how can i help you today",
    "how can i help you",
    "i hope you are doing well",
    "i hope you're doing well",
    "i'd be glad to help",
    "i'd be happy to help",
    "happy to help",
    "i hope this helps",
    "hope this helps",
    "you're welcome",
    "good morning",
    "good afternoon",
    "good evening",
    "sure thing",
    "of course",
    "thank you",
    "no problem",
    "hello there",
    "hello",
    "hi there",
    "hi",
    "hey",
    "absolutely",
    "certainly",
    "sure",
    "thanks",
];

const NON_VERBAL_LEADS: &[&str] = &[
    "as an ai",
    "as a language model",
    "would you like me to",
    "let me know if",
];

fn invalid(reason: &'static str) -> EonError {
    EonError::ValidationError(reason.into())
}

fn separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | '.' | '!' | '?' | ':' | ';')
}

fn after_phrase<'a>(s: &'a str, phrase: &str) -> Option<&'a str> {
    let (head, tail) = s.split_at_checked(phrase.len())?;
    (head.eq_ignore_ascii_case(phrase) && (tail.is_empty() || tail.starts_with(separator)))
        .then_some(tail)
}

fn opener(s: &str) -> Option<(SpeechAct, &str)> {
    SpeechAct::ALL.into_iter().find_map(|act| {
        let label = &act.prefix()[..act.prefix().len() - 1];
        let (head, rest) = s.split_at_checked(label.len())?;
        if !head.eq_ignore_ascii_case(label) {
            return None;
        }
        let rest = rest.trim_start();
        // Accept a canonical prefix without a following space, but don't treat report.csv as a label.
        let tail = rest.strip_prefix(':').or_else(|| {
            rest.strip_prefix('.').filter(|s| {
                act.prefix().ends_with('.') || s.is_empty() || s.starts_with(char::is_whitespace)
            })
        })?;
        Some((act, tail))
    })
}

/// Emit one canonical prefix. Repeating this operation is idempotent. Leading stock
/// pleasantries and repeats of the selected prefix are removed. A conflicting label, embedded
/// boilerplate or control markup rejects the utterance rather than rewriting its meaning.
/// Only an acknowledgment may stand alone. For model token pieces, use `router::SpeechRouter`.
pub fn format_speech(act: SpeechAct, public_body: &str) -> EonResult<String> {
    if public_body.contains(['<', '>'])
        || public_body
            .chars()
            .any(|c| c.is_control() && !c.is_whitespace())
    {
        return Err(invalid("expected plain verbal text, not control markup"));
    }
    let normalized = public_body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut body = normalized.as_str();
    loop {
        if let Some((found, rest)) = opener(body) {
            if found != act {
                return Err(invalid("conflicting Ciel speech acts"));
            }
            body = rest.trim_start();
        } else if let Some(rest) = FILLER.iter().find_map(|phrase| {
            after_phrase(body, phrase)
                .map(str::trim_start)
                .filter(|s| s.is_empty() || s.starts_with(separator))
        }) {
            body = rest.trim_start_matches(separator);
        } else {
            break;
        }
    }
    if body.is_empty() {
        return if act == SpeechAct::Affirmative {
            Ok(act.prefix().into())
        } else {
            Err(invalid("this speech act requires a public body"))
        };
    }
    if !body.chars().any(char::is_alphanumeric) {
        return Err(invalid("empty verbal content"));
    }

    // Never splice words out of a finding. Ordinary words in context ("make sure", "report.csv")
    // aren't new clauses or speech acts.
    let boilerplate = body.split([',', '.', '!', '?', ':', ';']).any(|clause| {
        FILLER
            .iter()
            .chain(NON_VERBAL_LEADS)
            .any(|p| after_phrase(clause.trim(), p).is_some())
    });
    let another_opener = body
        .match_indices(separator)
        .any(|(i, sep)| opener(&body[i + sep.len()..]).is_some());
    if boilerplate || another_opener {
        return Err(invalid(
            "unexpected prefix or conversational boilerplate in verbal body",
        ));
    }

    // Drop exact adjacent repeated sentences, not repeated words, decimal points or filenames.
    let mut speech = act.prefix().to_owned();
    let mut last = "";
    let mut start = 0;
    let ends = body
        .match_indices(['.', '!', '?'])
        .map(|(i, _)| i + 1)
        .filter(|&end| body[end..].starts_with(' '));
    for end in ends.chain(std::iter::once(body.len())) {
        let sentence = body[start..end].trim();
        if sentence != last {
            speech.push(' ');
            speech.push_str(sentence);
        }
        last = sentence;
        start = end;
    }
    Ok(speech)
}
