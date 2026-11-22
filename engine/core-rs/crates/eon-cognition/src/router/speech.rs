//! The only output channel for a routed turn is validated public speech.

use crate::persona::{SpeechAct, format_speech};
use eon_primitives::{EonError, EonResult};
use std::time::Instant;

use super::{Route, SpeechBudget};

const OPEN: &[u8] = b"<think>";
const CLOSE: &[u8] = b"</think>";
const MAX_SPEECH_BYTES: usize = 16 * 1024;
const MAX_TURN_BYTES: usize = 64 * 1024;

/// Consumes raw model pieces, including special tokens. A turn is optional leading `<think>`
/// blocks followed by the requested canonical opener and its public body. Other markup, stray
/// closers, late/nested blocks and incomplete turns fail closed. No bytes leave before `finish`.
/// Marker holdback is just an index; hidden content is neither accumulated nor returned.
pub struct SpeechRouter {
    act: SpeechAct,
    hidden: bool,
    matched: usize,
    speech: Vec<u8>,
    failed: bool,
}

/// Backend write-only sink. Each call consumes one generated (non-EOG) token,
/// including hidden and special tokens. Ignoring a rejected write cannot release speech.
pub struct SpeechTokens<'a> {
    router: SpeechRouter,
    remaining_tokens: usize,
    remaining_bytes: usize,
    route: Route,
    budget: &'a SpeechBudget,
}

impl<'a> SpeechTokens<'a> {
    pub(super) fn new(
        act: SpeechAct,
        max_tokens: usize,
        route: Route,
        budget: &'a SpeechBudget,
    ) -> Self {
        Self {
            router: SpeechRouter::new(act),
            remaining_tokens: max_tokens,
            remaining_bytes: MAX_TURN_BYTES,
            route,
            budget,
        }
    }

    pub fn deadline(&self) -> Instant {
        self.budget.deadline
    }

    /// Also available during prompt evaluation, before there are tokens to write.
    pub fn check_active(&self) -> EonResult<()> {
        self.budget
            .check(self.route)
            .map_err(|why| EonError::OperationFailed {
                operation: "speech".into(),
                reason: why.to_string(),
            })
    }

    pub fn push_token(&mut self, bytes: &[u8]) -> EonResult<()> {
        self.check_active()?;
        if self.remaining_tokens == 0 || bytes.len() > self.remaining_bytes {
            self.router.failed = true;
            return Err(invalid());
        }
        self.remaining_tokens -= 1;
        self.remaining_bytes -= bytes.len();
        self.router.push(bytes)
    }

    pub(super) fn finish(self) -> EonResult<String> {
        self.router.finish()
    }
}

fn invalid() -> EonError {
    EonError::ValidationError("invalid or incomplete verbal turn".into())
}

impl SpeechRouter {
    pub fn new(act: SpeechAct) -> Self {
        Self {
            act,
            hidden: false,
            matched: 0,
            speech: Vec::new(),
            failed: false,
        }
    }

    pub fn push(&mut self, piece: &[u8]) -> EonResult<()> {
        if self.failed || !piece.iter().all(|&b| self.consume(b)) {
            self.failed = true;
            self.speech.clear();
            return Err(invalid());
        }
        Ok(())
    }

    fn consume(&mut self, b: u8) -> bool {
        if self.matched > 0 || b == b'<' {
            let marker = if self.hidden { CLOSE } else { OPEN };
            if !self.speech.is_empty() || b != marker[self.matched] {
                return false;
            }
            self.matched += 1;
            if self.matched == marker.len() {
                self.hidden = !self.hidden;
                self.matched = 0;
            }
        } else if !self.hidden {
            if self.speech.is_empty() && b.is_ascii_whitespace() {
                return true;
            }
            if b == b'>'
                || self.speech.len() == MAX_SPEECH_BYTES
                || self
                    .act
                    .prefix()
                    .as_bytes()
                    .get(self.speech.len())
                    .is_some_and(|&p| p != b)
            {
                return false;
            }
            self.speech.push(b);
        }
        true
    }

    pub fn finish(self) -> EonResult<String> {
        if self.failed
            || self.hidden
            || self.matched != 0
            || self.speech.len() < self.act.prefix().len()
        {
            return Err(invalid());
        }
        format_speech(
            self.act,
            std::str::from_utf8(&self.speech).map_err(|_| invalid())?,
        )
    }
}
