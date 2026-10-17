//! Control-frame payload codecs (SPEC §5 "Control frame payload layouts").
//!
//! Byte-level encode/decode for the control payloads: `ACK`, `NACK`,
//! `RESUME` / `RESUME_RESPONSE` (two directions, two frame types),
//! `HEARTBEAT`, `ERROR`. All integers are big-endian; every encoded payload
//! is bounded by the 16 KiB control-frame cap (§4).
//!
//! Scope: pure codec. Decoding is strict — counts outside their declared
//! range, truncated or trailing bytes, and unknown RESUME status values are
//! [`ControlError`]s. Errors live in a separate channel from the frame-level
//! [`crate::frame::FrameError`]: this layer validates payload semantics
//! (counts, entry lengths, enum values), not framing.
//!
//! # Payload ↔ frame type pairing
//!
//! Each payload type carries its wire frame type as a `FRAME_TYPE`
//! associated constant — the single point of binding (the xacpp-ts
//! equivalent is `CONTROL_PAYLOAD_FRAME_TYPES`; values MUST stay in sync).
//! Two directions never share a frame type — dispatch on `frame_type`,
//! never on payload shape.

use crate::frame::{FrameType, MAX_CONTROL_FRAME_LEN};

/// Control payload byte cap (§4 control-frame limit; identical bound).
pub const CONTROL_PAYLOAD_CAP: usize = MAX_CONTROL_FRAME_LEN;

/// Protocol-level error code registry (SPEC §5, append-only: existing codes
/// are never renumbered or reused).
pub mod error_codes {
    /// Peer speaks an unsupported protocol version.
    pub const PROTOCOL_VERSION_UNSUPPORTED: u16 = 0x0001;
    /// Transaction copy expired on the holder side.
    pub const TRANSACTION_EXPIRED: u16 = 0x0002;
    /// Sender is stalled by the backpressure cap.
    pub const BACKPRESSURE_STALL: u16 = 0x0003;
}

/// One ACK progress entry — 24 bytes on the wire
/// (16 B transaction id + u32 + u32, big-endian).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckEntry {
    /// UUID of the owning transaction (16 raw bytes).
    pub transaction_id: [u8; 16],
    /// All frames `0..=seq` are received (cumulative). A receiver whose
    /// frames lack a contiguous prefix from frame 0 exports no ACK entry
    /// for that transaction at all (SPEC §5) — the u32 has no "no prefix"
    /// encoding; NACK gap lists cover that case.
    pub highest_contiguous_seq: u32,
    /// Total distinct frames received (detects sparse residue).
    pub received_frames: u32,
}

const ACK_ENTRY_LEN: usize = 24;
const RESUME_STATUS_ENTRY_LEN: usize = 17;

impl AckPayload {
    /// Wire frame type for this payload (payload ↔ frame type pairing).
    pub const FRAME_TYPE: FrameType = FrameType::Ack;
}

/// ACK payload — cumulative transaction progress receipt.
///
/// Wire layout: `u8 count (1..=255)` + `count` × 24-byte [`AckEntry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckPayload {
    /// Progress entries (1..=255).
    pub entries: Vec<AckEntry>,
}

impl NackPayload {
    /// Wire frame type for this payload (payload ↔ frame type pairing).
    pub const FRAME_TYPE: FrameType = FrameType::Nack;
}

/// NACK entry — one transaction's missing-frame list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NackEntry {
    /// UUID of the owning transaction (16 raw bytes).
    pub transaction_id: [u8; 16],
    /// Missing frame sequence numbers, ascending on the wire.
    pub missing_seqs: Vec<u32>,
}

/// NACK payload — missing-frame request.
///
/// Wire layout: `u8 count (1..=255)` + per entry
/// `u8[16] transaction_id + u16 missing_count + missing_count × u32`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NackPayload {
    /// Gap-report entries (1..=255).
    pub entries: Vec<NackEntry>,
}

impl ResumeRequestPayload {
    /// Wire frame type for this payload (payload ↔ frame type pairing).
    pub const FRAME_TYPE: FrameType = FrameType::Resume;
}

/// RESUME request payload — receiver declares progress so the copy holder
/// can answer per entry and retransmit missing frames.
///
/// Wire layout: `u8 count (0..=255; 0 = nothing to resume)` + `count` ×
/// 24-byte [`AckEntry`] (ACK entry shape per SPEC §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRequestPayload {
    /// Progress entries (0..=255).
    pub entries: Vec<AckEntry>,
}

/// RESUME response status (SPEC §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeStatus {
    /// `0x01` — retransmission follows.
    Resumed,
    /// `0x02` — copy no longer held; receiver discards the transaction.
    Rejected,
    /// `0x03` — transaction unknown to the holder.
    Unknown,
}

impl ResumeStatus {
    /// Wire u8 value.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Resumed => 0x01,
            Self::Rejected => 0x02,
            Self::Unknown => 0x03,
        }
    }

    /// Classify a wire u8 value; values outside the §5 registry are
    /// [`ControlError::UnknownResumeStatus`].
    pub fn from_u8(value: u8) -> Result<Self, ControlError> {
        match value {
            0x01 => Ok(Self::Resumed),
            0x02 => Ok(Self::Rejected),
            0x03 => Ok(Self::Unknown),
            other => Err(ControlError::UnknownResumeStatus { value: other }),
        }
    }
}

/// One RESUME response entry — 17 bytes on the wire (16 B id + u8 status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeStatusEntry {
    /// UUID of the owning transaction (16 raw bytes).
    pub transaction_id: [u8; 16],
    /// Holder's answer for this transaction.
    pub status: ResumeStatus,
}

impl ResumeResponsePayload {
    /// Wire frame type for this payload (payload ↔ frame type pairing).
    pub const FRAME_TYPE: FrameType = FrameType::ResumeResponse;
}

/// RESUME response payload — holder answers per request entry.
///
/// Wire layout: `u8 count (0..=255)` + `count` × [`ResumeStatusEntry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeResponsePayload {
    /// Status entries (0..=255).
    pub entries: Vec<ResumeStatusEntry>,
}

impl HeartbeatPayload {
    /// Wire frame type for this payload (payload ↔ frame type pairing).
    pub const FRAME_TYPE: FrameType = FrameType::Heartbeat;
}

/// HEARTBEAT payload — unidirectional liveness probe (no echo).
///
/// Wire layout: `u32 monotonic_seq + u64 sender_epoch_ms` (12 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatPayload {
    /// Sender-local monotonically increasing sequence.
    pub monotonic_seq: u32,
    /// Sender wall-clock epoch milliseconds.
    pub sender_epoch_ms: u64,
}

impl ErrorPayload {
    /// Wire frame type for this payload (payload ↔ frame type pairing).
    pub const FRAME_TYPE: FrameType = FrameType::Error;
}

/// ERROR payload — protocol-level error.
///
/// Wire layout: `u16 code + u8[] message` (UTF-8, may be empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorPayload {
    /// Registry code (see [`error_codes`]; registry is append-only).
    pub code: u16,
    /// Human-readable detail (UTF-8, may be empty).
    pub message: String,
}

/// Control payload codec error — separate channel from the frame-level
/// [`crate::frame::FrameError`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    /// Entry count outside the range declared for this payload type.
    #[error("entry count {count} out of range {min}..={max}")]
    CountOutOfRange {
        /// Actual count.
        count: usize,
        /// Inclusive lower bound.
        min: usize,
        /// Inclusive upper bound.
        max: usize,
    },
    /// Wire bytes do not exactly match the layout implied by `count`
    /// (truncated payload or trailing bytes).
    #[error("payload length mismatch: expected {expected} bytes, got {actual}")]
    LengthMismatch {
        /// Byte count implied by the declared count.
        expected: usize,
        /// Bytes actually present.
        actual: usize,
    },
    /// Missing-seq list does not fit the u16 `missing_count` field.
    #[error("missing-seq list too long: {len} entries (u16 max {max})")]
    MissingSeqOverflow {
        /// Actual list length.
        len: usize,
        /// `u16::MAX`.
        max: usize,
    },
    /// Encoded payload exceeds the 16 KiB control-frame cap (§4).
    #[error("encoded payload too large: {actual} bytes (cap {cap})")]
    PayloadTooLarge {
        /// Encoded byte count.
        actual: usize,
        /// [`CONTROL_PAYLOAD_CAP`].
        cap: usize,
    },
    /// RESUME response status outside the §5 registry (`0x01..=0x03`).
    #[error("unknown resume status: {value:#04x}")]
    UnknownResumeStatus {
        /// Offending status byte.
        value: u8,
    },
    /// ERROR message bytes are not valid UTF-8.
    #[error("error message is not valid UTF-8")]
    InvalidUtf8,
    /// HEARTBEAT payload is not exactly 12 bytes.
    #[error("heartbeat payload must be exactly 12 bytes, got {actual}")]
    HeartbeatLength {
        /// Actual byte count.
        actual: usize,
    },
}

/// Shared post-encode bound check (§4 control-frame cap).
fn finish(out: Vec<u8>) -> Result<Vec<u8>, ControlError> {
    if out.len() > CONTROL_PAYLOAD_CAP {
        return Err(ControlError::PayloadTooLarge {
            actual: out.len(),
            cap: CONTROL_PAYLOAD_CAP,
        });
    }
    Ok(out)
}

/// Entry-count range check used by both encode (Vec length) and decode
/// (wire count byte).
fn check_count(count: usize, min: usize, max: usize) -> Result<(), ControlError> {
    if count < min || count > max {
        return Err(ControlError::CountOutOfRange { count, min, max });
    }
    Ok(())
}

impl AckPayload {
    /// Encode (big-endian, §5 ACK layout).
    pub fn encode(&self) -> Result<Vec<u8>, ControlError> {
        check_count(self.entries.len(), 1, 255)?;
        let mut out = Vec::with_capacity(1 + self.entries.len() * ACK_ENTRY_LEN);
        out.push(self.entries.len() as u8);
        write_ack_entries(&mut out, &self.entries);
        finish(out)
    }

    /// Decode; rejects counts outside `1..=255`, truncated payloads and
    /// trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, ControlError> {
        let Some((&count_byte, rest)) = bytes.split_first() else {
            return Err(ControlError::LengthMismatch {
                expected: 1,
                actual: 0,
            });
        };
        let count = count_byte as usize;
        check_count(count, 1, 255)?;
        let expected = count * ACK_ENTRY_LEN;
        if rest.len() != expected {
            return Err(ControlError::LengthMismatch {
                expected,
                actual: rest.len(),
            });
        }
        Ok(Self {
            entries: read_ack_entries(rest, count),
        })
    }
}

/// Write `count` × 24-byte ACK entries.
fn write_ack_entries(out: &mut Vec<u8>, entries: &[AckEntry]) {
    for entry in entries {
        out.extend_from_slice(&entry.transaction_id);
        out.extend_from_slice(&entry.highest_contiguous_seq.to_be_bytes());
        out.extend_from_slice(&entry.received_frames.to_be_bytes());
    }
}

/// Parse exactly `count` × 24-byte ACK entries from `bytes`
/// (`bytes.len() == count * ACK_ENTRY_LEN`, caller-checked).
fn read_ack_entries(bytes: &[u8], count: usize) -> Vec<AckEntry> {
    bytes
        .chunks_exact(ACK_ENTRY_LEN)
        .take(count)
        .map(|chunk| AckEntry {
            transaction_id: chunk[0..16].try_into().expect("16-byte slice"),
            highest_contiguous_seq: u32::from_be_bytes(
                chunk[16..20].try_into().expect("4-byte slice"),
            ),
            received_frames: u32::from_be_bytes(chunk[20..24].try_into().expect("4-byte slice")),
        })
        .collect()
}

impl NackPayload {
    /// Encode (big-endian, §5 NACK layout).
    pub fn encode(&self) -> Result<Vec<u8>, ControlError> {
        check_count(self.entries.len(), 1, 255)?;
        let mut out = vec![self.entries.len() as u8];
        for entry in &self.entries {
            let len = entry.missing_seqs.len();
            if len > u16::MAX as usize {
                return Err(ControlError::MissingSeqOverflow {
                    len,
                    max: u16::MAX as usize,
                });
            }
            out.extend_from_slice(&entry.transaction_id);
            out.extend_from_slice(&(len as u16).to_be_bytes());
            for seq in &entry.missing_seqs {
                out.extend_from_slice(&seq.to_be_bytes());
            }
        }
        finish(out)
    }

    /// Decode; rejects counts outside `1..=255` and payloads that do not
    /// exactly match the declared layout (truncation or trailing bytes).
    pub fn decode(bytes: &[u8]) -> Result<Self, ControlError> {
        let Some((&count_byte, mut rest)) = bytes.split_first() else {
            return Err(ControlError::LengthMismatch {
                expected: 1,
                actual: 0,
            });
        };
        let count = count_byte as usize;
        check_count(count, 1, 255)?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            if rest.len() < 18 {
                return Err(ControlError::LengthMismatch {
                    expected: 18,
                    actual: rest.len(),
                });
            }
            let mut entry = NackEntry {
                transaction_id: rest[0..16].try_into().expect("16-byte slice"),
                missing_seqs: Vec::new(),
            };
            let missing_count = u16::from_be_bytes([rest[16], rest[17]]) as usize;
            rest = &rest[18..];
            let expected = missing_count * 4;
            if rest.len() < expected {
                return Err(ControlError::LengthMismatch {
                    expected,
                    actual: rest.len(),
                });
            }
            entry.missing_seqs = rest[..expected]
                .chunks_exact(4)
                .map(|c| u32::from_be_bytes(c.try_into().expect("4-byte slice")))
                .collect();
            rest = &rest[expected..];
            entries.push(entry);
        }
        if !rest.is_empty() {
            return Err(ControlError::LengthMismatch {
                expected: 0,
                actual: rest.len(),
            });
        }
        Ok(Self { entries })
    }
}

impl ResumeRequestPayload {
    /// Encode (big-endian, §5 RESUME request layout).
    pub fn encode(&self) -> Result<Vec<u8>, ControlError> {
        check_count(self.entries.len(), 0, 255)?;
        let mut out = Vec::with_capacity(1 + self.entries.len() * ACK_ENTRY_LEN);
        out.push(self.entries.len() as u8);
        write_ack_entries(&mut out, &self.entries);
        finish(out)
    }

    /// Decode; `count == 0` is legal (nothing to resume).
    pub fn decode(bytes: &[u8]) -> Result<Self, ControlError> {
        let Some((&count_byte, rest)) = bytes.split_first() else {
            return Err(ControlError::LengthMismatch {
                expected: 1,
                actual: 0,
            });
        };
        let count = count_byte as usize;
        check_count(count, 0, 255)?;
        let expected = count * ACK_ENTRY_LEN;
        if rest.len() != expected {
            return Err(ControlError::LengthMismatch {
                expected,
                actual: rest.len(),
            });
        }
        Ok(Self {
            entries: read_ack_entries(rest, count),
        })
    }
}

impl ResumeResponsePayload {
    /// Encode (big-endian, §5 RESUME response layout).
    pub fn encode(&self) -> Result<Vec<u8>, ControlError> {
        check_count(self.entries.len(), 0, 255)?;
        let mut out = Vec::with_capacity(1 + self.entries.len() * RESUME_STATUS_ENTRY_LEN);
        out.push(self.entries.len() as u8);
        for entry in &self.entries {
            out.extend_from_slice(&entry.transaction_id);
            out.push(entry.status.to_u8());
        }
        finish(out)
    }

    /// Decode; unknown status bytes are [`ControlError::UnknownResumeStatus`].
    pub fn decode(bytes: &[u8]) -> Result<Self, ControlError> {
        let Some((&count_byte, rest)) = bytes.split_first() else {
            return Err(ControlError::LengthMismatch {
                expected: 1,
                actual: 0,
            });
        };
        let count = count_byte as usize;
        check_count(count, 0, 255)?;
        let expected = count * RESUME_STATUS_ENTRY_LEN;
        if rest.len() != expected {
            return Err(ControlError::LengthMismatch {
                expected,
                actual: rest.len(),
            });
        }
        let mut entries = Vec::with_capacity(count);
        for chunk in rest.chunks_exact(RESUME_STATUS_ENTRY_LEN) {
            entries.push(ResumeStatusEntry {
                transaction_id: chunk[0..16].try_into().expect("16-byte slice"),
                status: ResumeStatus::from_u8(chunk[16])?,
            });
        }
        Ok(Self { entries })
    }
}

impl HeartbeatPayload {
    /// Encode (12 bytes, big-endian).
    pub fn encode(&self) -> Result<Vec<u8>, ControlError> {
        let mut out = Vec::with_capacity(12);
        out.extend_from_slice(&self.monotonic_seq.to_be_bytes());
        out.extend_from_slice(&self.sender_epoch_ms.to_be_bytes());
        finish(out)
    }

    /// Decode; rejects payloads that are not exactly 12 bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, ControlError> {
        if bytes.len() != 12 {
            return Err(ControlError::HeartbeatLength {
                actual: bytes.len(),
            });
        }
        Ok(Self {
            monotonic_seq: u32::from_be_bytes(bytes[0..4].try_into().expect("4-byte slice")),
            sender_epoch_ms: u64::from_be_bytes(bytes[4..12].try_into().expect("8-byte slice")),
        })
    }
}

impl ErrorPayload {
    /// Encode (big-endian; message as UTF-8 bytes).
    pub fn encode(&self) -> Result<Vec<u8>, ControlError> {
        let mut out = Vec::with_capacity(2 + self.message.len());
        out.extend_from_slice(&self.code.to_be_bytes());
        out.extend_from_slice(self.message.as_bytes());
        finish(out)
    }

    /// Decode; message must be valid UTF-8 (may be empty).
    pub fn decode(bytes: &[u8]) -> Result<Self, ControlError> {
        if bytes.len() < 2 {
            return Err(ControlError::LengthMismatch {
                expected: 2,
                actual: bytes.len(),
            });
        }
        Ok(Self {
            code: u16::from_be_bytes([bytes[0], bytes[1]]),
            message: String::from_utf8(bytes[2..].to_vec())
                .map_err(|_| ControlError::InvalidUtf8)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txid(seed: u8) -> [u8; 16] {
        let mut id = [0u8; 16];
        id[0] = seed;
        id[15] = seed;
        id
    }

    fn ack_entry(seed: u8, highest: u32, received: u32) -> AckEntry {
        AckEntry {
            transaction_id: txid(seed),
            highest_contiguous_seq: highest,
            received_frames: received,
        }
    }

    // ---- ACK ----

    #[test]
    fn ack_roundtrip() {
        let payload = AckPayload {
            entries: vec![
                ack_entry(1, 0, 1),
                ack_entry(2, 41, 63),
                ack_entry(3, u32::MAX, u32::MAX),
            ],
        };
        let decoded = AckPayload::decode(&payload.encode().unwrap()).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn ack_wire_layout_big_endian() {
        let payload = AckPayload {
            entries: vec![AckEntry {
                transaction_id: txid(7),
                highest_contiguous_seq: 0x01020304,
                received_frames: 0x05060708,
            }],
        };
        let bytes = payload.encode().unwrap();
        assert_eq!(bytes.len(), 1 + 24);
        assert_eq!(bytes[0], 1); // count
        assert_eq!(&bytes[1..17], &txid(7));
        assert_eq!(&bytes[17..21], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(&bytes[21..25], &[0x05, 0x06, 0x07, 0x08]);
    }

    #[test]
    fn ack_count_bounds() {
        // encode: count 0 rejected
        let empty = AckPayload { entries: vec![] };
        assert!(matches!(
            empty.encode(),
            Err(ControlError::CountOutOfRange {
                count: 0,
                min: 1,
                max: 255
            })
        ));
        // encode: count 256 rejected
        let too_many = AckPayload {
            entries: vec![ack_entry(1, 0, 0); 256],
        };
        assert!(matches!(
            too_many.encode(),
            Err(ControlError::CountOutOfRange {
                count: 256,
                min: 1,
                max: 255
            })
        ));
        // decode: wire count byte 0 rejected
        assert!(matches!(
            AckPayload::decode(&[0]),
            Err(ControlError::CountOutOfRange {
                count: 0,
                min: 1,
                max: 255
            })
        ));
    }

    #[test]
    fn ack_max_count_roundtrip() {
        let payload = AckPayload {
            entries: vec![ack_entry(9, 5, 5); 255],
        };
        let bytes = payload.encode().unwrap();
        assert_eq!(bytes.len(), 1 + 255 * 24);
        assert_eq!(AckPayload::decode(&bytes).unwrap(), payload);
    }

    #[test]
    fn ack_truncated_and_trailing_rejected() {
        let full = AckPayload {
            entries: vec![ack_entry(1, 3, 7), ack_entry(2, 1, 2)],
        }
        .encode()
        .unwrap();
        // truncated
        let err = AckPayload::decode(&full[..full.len() - 1]).unwrap_err();
        assert!(matches!(err, ControlError::LengthMismatch { .. }));
        // trailing byte
        let mut trailing = full.clone();
        trailing.push(0);
        let err = AckPayload::decode(&trailing).unwrap_err();
        assert!(matches!(err, ControlError::LengthMismatch { .. }));
        // empty
        let err = AckPayload::decode(&[]).unwrap_err();
        assert!(matches!(
            err,
            ControlError::LengthMismatch {
                expected: 1,
                actual: 0
            }
        ));
    }

    // ---- NACK ----

    #[test]
    fn nack_roundtrip() {
        let payload = NackPayload {
            entries: vec![
                NackEntry {
                    transaction_id: txid(1),
                    missing_seqs: vec![2, 5, 9],
                },
                NackEntry {
                    transaction_id: txid(2),
                    missing_seqs: vec![],
                },
            ],
        };
        let decoded = NackPayload::decode(&payload.encode().unwrap()).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn nack_wire_layout_big_endian() {
        let payload = NackPayload {
            entries: vec![NackEntry {
                transaction_id: txid(3),
                missing_seqs: vec![0x0102_0304],
            }],
        };
        let bytes = payload.encode().unwrap();
        assert_eq!(bytes.len(), 1 + 16 + 2 + 4);
        assert_eq!(bytes[0], 1);
        assert_eq!(&bytes[1..17], &txid(3));
        assert_eq!(&bytes[17..19], &[0x00, 0x01]); // missing_count
        assert_eq!(&bytes[19..23], &[0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn nack_count_bounds() {
        let empty = NackPayload { entries: vec![] };
        assert!(matches!(
            empty.encode(),
            Err(ControlError::CountOutOfRange {
                count: 0,
                min: 1,
                max: 255
            })
        ));
        assert!(matches!(
            NackPayload::decode(&[0]),
            Err(ControlError::CountOutOfRange {
                count: 0,
                min: 1,
                max: 255
            })
        ));
    }

    #[test]
    fn nack_missing_count_u16_overflow() {
        let payload = NackPayload {
            entries: vec![NackEntry {
                transaction_id: txid(1),
                missing_seqs: vec![0; u16::MAX as usize + 1],
            }],
        };
        assert!(matches!(
            payload.encode(),
            Err(ControlError::MissingSeqOverflow {
                len: 65_536,
                max: 65_535
            })
        ));
    }

    #[test]
    fn nack_large_list_within_cap_roundtrip() {
        // 4000 seqs → 1 + 16 + 2 + 16000 = 16019 bytes ≤ 16 KiB.
        let payload = NackPayload {
            entries: vec![NackEntry {
                transaction_id: txid(4),
                missing_seqs: (0..4000u32).collect(),
            }],
        };
        let bytes = payload.encode().unwrap();
        assert_eq!(bytes.len(), 16_019);
        assert_eq!(NackPayload::decode(&bytes).unwrap(), payload);
    }

    #[test]
    fn nack_over_cap_rejected_on_encode() {
        // 5000 seqs → 1 + 16 + 2 + 20000 = 20019 bytes > 16 KiB.
        let payload = NackPayload {
            entries: vec![NackEntry {
                transaction_id: txid(4),
                missing_seqs: (0..5000u32).collect(),
            }],
        };
        assert!(matches!(
            payload.encode(),
            Err(ControlError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn nack_truncated_rejected() {
        let full = NackPayload {
            entries: vec![NackEntry {
                transaction_id: txid(1),
                missing_seqs: vec![1, 2, 3],
            }],
        }
        .encode()
        .unwrap();
        // Cut inside the seq list (declared missing_count no longer backed).
        let err = NackPayload::decode(&full[..full.len() - 2]).unwrap_err();
        assert!(matches!(err, ControlError::LengthMismatch { .. }));
        // Cut inside the entry header.
        let err = NackPayload::decode(&full[..10]).unwrap_err();
        assert!(matches!(err, ControlError::LengthMismatch { .. }));
        // Trailing bytes after the last entry.
        let mut trailing = full.clone();
        trailing.push(0xAA);
        let err = NackPayload::decode(&trailing).unwrap_err();
        assert!(matches!(err, ControlError::LengthMismatch { .. }));
    }

    // ---- RESUME ----

    #[test]
    fn resume_request_roundtrip_including_zero_count() {
        // count 0 = nothing to resume; legal per SPEC §5.
        let empty = ResumeRequestPayload { entries: vec![] };
        let bytes = empty.encode().unwrap();
        assert_eq!(bytes, vec![0]);
        assert_eq!(ResumeRequestPayload::decode(&bytes).unwrap(), empty);

        let payload = ResumeRequestPayload {
            entries: vec![ack_entry(1, 12, 40), ack_entry(2, 0, 3)],
        };
        let decoded = ResumeRequestPayload::decode(&payload.encode().unwrap()).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn resume_request_bounds() {
        let too_many = ResumeRequestPayload {
            entries: vec![ack_entry(1, 0, 0); 256],
        };
        assert!(matches!(
            too_many.encode(),
            Err(ControlError::CountOutOfRange {
                count: 256,
                min: 0,
                max: 255
            })
        ));
    }

    #[test]
    fn resume_response_roundtrip() {
        let payload = ResumeResponsePayload {
            entries: vec![
                ResumeStatusEntry {
                    transaction_id: txid(1),
                    status: ResumeStatus::Resumed,
                },
                ResumeStatusEntry {
                    transaction_id: txid(2),
                    status: ResumeStatus::Rejected,
                },
                ResumeStatusEntry {
                    transaction_id: txid(3),
                    status: ResumeStatus::Unknown,
                },
            ],
        };
        let decoded = ResumeResponsePayload::decode(&payload.encode().unwrap()).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn resume_response_zero_count_legal() {
        let payload = ResumeResponsePayload { entries: vec![] };
        let bytes = payload.encode().unwrap();
        assert_eq!(bytes, vec![0]);
        assert_eq!(ResumeResponsePayload::decode(&bytes).unwrap(), payload);
    }

    #[test]
    fn resume_response_unknown_status_rejected_on_decode() {
        // 1 (count) + 16 (txid) + 1 (status) = 18 bytes.
        for bad in [0x00u8, 0x04, 0xFF] {
            let mut bytes = [0u8; 18];
            bytes[0] = 1;
            bytes[17] = bad;
            let err = ResumeResponsePayload::decode(&bytes).unwrap_err();
            assert!(matches!(err, ControlError::UnknownResumeStatus { .. }));
        }
    }

    #[test]
    fn resume_response_wire_layout() {
        let payload = ResumeResponsePayload {
            entries: vec![ResumeStatusEntry {
                transaction_id: txid(5),
                status: ResumeStatus::Rejected,
            }],
        };
        let bytes = payload.encode().unwrap();
        assert_eq!(bytes.len(), 1 + 17);
        assert_eq!(bytes[0], 1);
        assert_eq!(&bytes[1..17], &txid(5));
        assert_eq!(bytes[17], 0x02);
    }

    // ---- HEARTBEAT ----

    #[test]
    fn heartbeat_roundtrip() {
        let payload = HeartbeatPayload {
            monotonic_seq: 0x0102_0304,
            sender_epoch_ms: 0x1122_3344_5566_7788,
        };
        let bytes = payload.encode().unwrap();
        assert_eq!(bytes.len(), 12);
        assert_eq!(HeartbeatPayload::decode(&bytes).unwrap(), payload);
    }

    #[test]
    fn heartbeat_fixed_length_enforced() {
        let payload = HeartbeatPayload {
            monotonic_seq: 1,
            sender_epoch_ms: 2,
        };
        let bytes = payload.encode().unwrap();
        assert_eq!(bytes.len(), 12);
        // Empty and truncated.
        assert!(matches!(
            HeartbeatPayload::decode(&[]),
            Err(ControlError::HeartbeatLength { actual: 0 })
        ));
        assert!(matches!(
            HeartbeatPayload::decode(&bytes[..11]),
            Err(ControlError::HeartbeatLength { actual: 11 })
        ));
        // Trailing byte.
        let mut longer = bytes.clone();
        longer.push(0xAA);
        assert!(matches!(
            HeartbeatPayload::decode(&longer),
            Err(ControlError::HeartbeatLength { actual: 13 })
        ));
    }

    // ---- ERROR ----

    #[test]
    fn error_roundtrip_empty_and_nonempty_message() {
        for message in ["", "transaction expired"] {
            let payload = ErrorPayload {
                code: error_codes::TRANSACTION_EXPIRED,
                message: message.to_string(),
            };
            let bytes = payload.encode().unwrap();
            assert_eq!(ErrorPayload::decode(&bytes).unwrap(), payload);
        }
    }

    #[test]
    fn error_wire_layout() {
        let payload = ErrorPayload {
            code: 0x0003,
            message: "stalled".to_string(),
        };
        let bytes = payload.encode().unwrap();
        assert_eq!(&bytes[0..2], &[0x00, 0x03]);
        assert_eq!(&bytes[2..], b"stalled");
    }

    #[test]
    fn error_invalid_utf8_rejected() {
        let bytes = [0x00, 0x01, 0xFF, 0xFE];
        assert!(matches!(
            ErrorPayload::decode(&bytes),
            Err(ControlError::InvalidUtf8)
        ));
    }

    #[test]
    fn error_too_short_rejected() {
        assert!(matches!(
            ErrorPayload::decode(&[0x00]),
            Err(ControlError::LengthMismatch {
                expected: 2,
                actual: 1
            })
        ));
        assert!(matches!(
            ErrorPayload::decode(&[]),
            Err(ControlError::LengthMismatch {
                expected: 2,
                actual: 0
            })
        ));
    }

    #[test]
    fn error_code_registry_values() {
        assert_eq!(error_codes::PROTOCOL_VERSION_UNSUPPORTED, 0x0001);
        assert_eq!(error_codes::TRANSACTION_EXPIRED, 0x0002);
        assert_eq!(error_codes::BACKPRESSURE_STALL, 0x0003);
    }
}
