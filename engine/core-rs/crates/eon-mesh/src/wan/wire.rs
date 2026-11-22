use prost::Message;

use super::{EonResult, MAX_AUDIO, TrafficClass, invalid};

// Data-only bindings for the matching messages in proto/eon.proto. No RPC
// service is generated, and no execution/PTY message is part of this ALPN.
#[derive(Clone, PartialEq, Message)]
pub struct AudioPcmChunk {
    #[prost(int64, tag = "1")]
    pub sequence_number: i64,
    #[prost(int64, tag = "2")]
    pub timestamp_ms: i64,
    #[prost(bytes = "vec", tag = "3")]
    pub pcm_data: Vec<u8>,
    #[prost(int32, tag = "4")]
    pub priority_class: i32,
}

impl AudioPcmChunk {
    pub(super) fn validate(&self) -> EonResult<()> {
        if self.priority_class != 0
            || self.pcm_data.is_empty()
            || !self.pcm_data.len().is_multiple_of(2)
            || self.encoded_len() > MAX_AUDIO
        {
            return Err(invalid("expected one bounded Class 0 PCM16 chunk"));
        }

        Ok(())
    }
}

pub(super) fn decode_audio(bytes: bytes::Bytes) -> EonResult<AudioPcmChunk> {
    if bytes.len() > MAX_AUDIO {
        return Err(invalid("oversized audio datagram"));
    }
    let pcm = AudioPcmChunk::decode(bytes).map_err(super::transport_error)?;
    pcm.validate()?;
    Ok(pcm)
}

#[derive(Clone, PartialEq, Message)]
pub struct EpochInvalidationEvent {
    #[prost(uint64, tag = "1")]
    pub invalidated_epoch: u64,
    #[prost(string, tag = "2")]
    pub task_id: String,
    #[prost(string, tag = "3")]
    pub reason: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct CrdtDeltaSync {
    #[prost(string, tag = "1")]
    pub ledger_id: String,
    #[prost(int64, tag = "2")]
    pub lamport_clock: i64,
    #[prost(bytes = "vec", tag = "3")]
    pub crdt_delta_payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReliableFrame {
    Control(EpochInvalidationEvent),
    Delta(CrdtDeltaSync),
    /// One chunk of an out-of-band agreed blob, not a file name or write
    /// request. Verify against a trusted [`crate::blob::Manifest`] before use.
    Bulk {
        index: u32,
        bytes: Vec<u8>,
    },
}

pub(super) const HEADER: usize = 5; // Class byte, then u32 LE body length.

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Lane {
    Control = 1,
    Delta = 2,
    Bulk = 3,
}

impl Lane {
    pub(super) fn class(self) -> TrafficClass {
        match self {
            Self::Control => TrafficClass::Control,
            Self::Delta => TrafficClass::Delta,
            Self::Bulk => TrafficClass::Bulk,
        }
    }

    pub(super) fn priority(self) -> i32 {
        self.class()
            .stream_priority()
            .expect("reliable lane has stream priority")
    }

    fn check(self, len: usize) -> EonResult<()> {
        let allowed = match self {
            Self::Control => 1..=1 << 20,
            Self::Delta => 1..=4 << 20,
            Self::Bulk => 5..=4 + 64 * 1024,
        };
        if !allowed.contains(&len) {
            return Err(invalid("invalid WAN frame size"));
        }
        Ok(())
    }

    pub(super) fn decode(self, payload: &[u8]) -> EonResult<ReliableFrame> {
        match self {
            Self::Control => EpochInvalidationEvent::decode(payload).map(ReliableFrame::Control),
            Self::Delta => CrdtDeltaSync::decode(payload).map(ReliableFrame::Delta),
            Self::Bulk => {
                let (index, bytes) = payload
                    .split_first_chunk()
                    .ok_or_else(|| invalid("missing chunk index"))?;
                return Ok(ReliableFrame::Bulk {
                    index: u32::from_le_bytes(*index),
                    bytes: bytes.to_vec(),
                });
            }
        }
        .map_err(|_| invalid("invalid WAN protobuf payload"))
    }
}

pub(super) fn parse_header(header: [u8; HEADER]) -> EonResult<(Lane, usize)> {
    let [class, a, b, c, d] = header;
    let lane = match class {
        1 => Lane::Control,
        2 => Lane::Delta,
        3 => Lane::Bulk,
        _ => return Err(invalid("unknown or non-stream WAN class")),
    };
    let len = u32::from_le_bytes([a, b, c, d]) as usize;
    lane.check(len)?;
    Ok((lane, len))
}

impl ReliableFrame {
    pub(super) fn checked_size(&self) -> EonResult<(Lane, usize)> {
        let (lane, len) = match self {
            Self::Control(msg) => (Lane::Control, msg.encoded_len()),
            Self::Delta(msg) => (Lane::Delta, msg.encoded_len()),
            Self::Bulk { bytes, .. } => (Lane::Bulk, bytes.len().saturating_add(4)),
        };
        lane.check(len)?;
        Ok((lane, len))
    }

    pub(super) fn encode(&self, lane: Lane, len: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER + len);
        bytes.push(lane as u8);
        bytes.extend_from_slice(&(len as u32).to_le_bytes());
        match self {
            Self::Control(msg) => msg.encode_raw(&mut bytes),
            Self::Delta(msg) => msg.encode_raw(&mut bytes),
            Self::Bulk { index, bytes: data } => {
                bytes.extend_from_slice(&index.to_le_bytes());
                bytes.extend_from_slice(data);
            }
        }
        bytes
    }
}
