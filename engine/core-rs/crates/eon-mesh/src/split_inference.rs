//! A bounded speech **decoding** stage, not an acoustic model. Given quantized
//! frame scores, run greedy CTC on the paired workstation or on the edge. Nothing
//! is played or executed. Verification deliberately trades duplicate work for
//! not having to trust the workstation's claimed tokens.
//!
//! The separate, opt-in `zipformer` service offloads acoustic inference over
//! PCM. `zipformer::readback` uses a different token-only protocol; it never
//! negotiates down to the PCM service.

#[cfg(feature = "native-zipformer")]
pub mod zipformer;

use std::time::Duration;

use eon_primitives::{EonError, EonResult};
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

use crate::paired::{Reply, Request, request, unpack};
use crate::transport::PeerNode;

const REMOTE_BUDGET: Duration = Duration::from_millis(500);
const MAX_FRAMES: usize = 1024;

/// Row-major unsigned scores, one row per frame. Label 0 is the CTC blank.
#[derive(Clone, Copy, Deserialize, Serialize)]
pub struct SpeechScores<'a> {
    pub vocabulary: u16,
    pub scores: &'a [u8],
}

impl SpeechScores<'_> {
    fn validate(&self) -> EonResult<()> {
        let width = usize::from(self.vocabulary);
        if !(2..=256).contains(&width)
            || self.scores.is_empty()
            || self.scores.len() > MAX_FRAMES * width
            || !self.scores.len().is_multiple_of(width)
        {
            return Err(EonError::ValidationError("invalid speech score matrix".into()));
        }
        Ok(())
    }

    pub fn decode(&self) -> EonResult<Vec<u8>> {
        self.validate()?;
        let mut tokens = Vec::new();
        let mut previous = 0;
        for frame in self.scores.chunks_exact(usize::from(self.vocabulary)) {
            let mut best = 0;
            for label in 1..frame.len() {
                if frame[label] > frame[best] {
                    best = label;
                }
            }
            if best != 0 && best != previous {
                tokens.push(best as u8);
            }
            previous = best;
        }
        Ok(tokens)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeSource {
    VerifiedPeer,
    LocalOnTimeout,
    LocalOnTransportError,
    LocalOnInvalidReply,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DecodedSpeech {
    pub tokens: Vec<u8>,
    pub source: DecodeSource,
}

/// The deadline covers dial, write and reply, not just connection establishment.
/// A bad result is discarded; both verification and fallback use the original input.
pub async fn decode_or_local(
    node: &PeerNode,
    peer: EndpointAddr,
    scores: SpeechScores<'_>,
) -> EonResult<DecodedSpeech> {
    if node.paired_id() != Some(peer.id) {
        return Err(EonError::SecurityViolation("expected the paired speech peer".into()));
    }

    scores.validate()?;
    let remote = tokio::time::timeout(REMOTE_BUDGET, request(node, peer, Request::Decode(scores))).await;
    let local = scores.decode()?;
    let source = match remote {
        Ok(Ok(bytes)) => match unpack(&bytes) {
            Ok(Reply::Tokens(tokens)) if tokens == local => {
                return Ok(DecodedSpeech {
                    tokens: tokens.to_vec(),
                    source: DecodeSource::VerifiedPeer,
                });
            }
            _ => DecodeSource::LocalOnInvalidReply,
        },
        Ok(Err(_)) => DecodeSource::LocalOnTransportError,
        Err(_) => DecodeSource::LocalOnTimeout,
    };
    Ok(DecodedSpeech { tokens: local, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_inference_tests_rejects_empty_ragged_or_oversized_score_matrices() {
        for (vocabulary, scores) in [(0, &[1, 2][..]), (3, &[1, 2]), (2, &[]), (257, &[1; 257])] {
            assert!(SpeechScores { vocabulary, scores }.decode().is_err());
        }
        let large = vec![0; (MAX_FRAMES + 1) * 2];
        assert!(
            SpeechScores {
                vocabulary: 2,
                scores: &large
            }
            .decode()
            .is_err()
        );
    }
}
