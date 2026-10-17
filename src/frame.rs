//! XACPP binary wire frame codec.
//!
//! Implements the v1.0 binary frame layer defined in `docs/SPEC.md`
//! (§3 Frame Format, §4 Frame Types): 36-byte fixed header + variable
//! payload + 4-byte CRC-32 trailer, big-endian throughout.
//!
//! Scope: pure synchronous codec — byte-level [`Frame::encode`] and an
//! incremental [`FrameDecoder`] for byte streams (TCP / stdio) handling
//! coalesced and split frames, with dirty-stream resynchronization via
//! magic scanning plus header field validation. Carriers, transaction
//! reassembly (ACK/NACK/RESUME handling) and async integration live in
//! upper layers.
//!
//! Buffer discipline: the decoder never pre-allocates from declared
//! lengths. A frame body is accumulated only after its header passed
//! validation, which caps any accumulation at the per-type frame size
//! limits (§4); garbage preceding a valid header is discarded continuously.

use crc32fast::Hasher;

/// Frame magic: ASCII `XACP`.
pub const MAGIC: [u8; 4] = *b"XACP";

/// Wire protocol major version. Mismatch ⇒ connection refused (SPEC §8).
pub const VERSION_MAJOR: u8 = 1;
/// Wire protocol minor version (backward-compatible extensions).
pub const VERSION_MINOR: u8 = 0;

/// Fixed header size in bytes (§3).
pub const HEADER_LEN: usize = 36;
/// CRC-32 trailer size in bytes.
pub const CRC_LEN: usize = 4;
/// Fixed per-frame overhead: header + CRC trailer (§3: 40 bytes).
pub const FRAME_OVERHEAD: usize = HEADER_LEN + CRC_LEN;

/// Max payload length for control frames (§4: 16 KiB).
pub const MAX_CONTROL_FRAME_LEN: usize = 16 * 1024;
/// Max payload length for `DATA_JSON` frames (§4: 1 MiB).
pub const MAX_DATA_JSON_FRAME_LEN: usize = 1024 * 1024;
/// Max payload length for `DATA_BINARY` frames (§4: 16 MiB).
pub const MAX_DATA_BINARY_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Decode buffer cap (§3 Rules): any frame whose total wire size (header +
/// payload + trailer) exceeds this bound is a protocol error regardless of
/// frame type. RECOMMENDED default per SPEC; bounds memory exposure to
/// unknown frame types and hostile declarations.
pub const MAX_DECODE_TOTAL_CAP: usize = 64 * 1024 * 1024;

/// Frame type identifier.
///
/// # Contract table (xacpp-rs / xacpp-ts must stay identical)
///
/// The u16 discriminants below are the wire contract shared by both
/// implementations. Control class occupies `0x0001..=0x00FF`, data class
/// `0x1001..=0x10FF`; do not renumber existing entries, only append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// Capability exchange and protocol feature negotiation.
    Negotiate,
    /// Receipt confirmation.
    Ack,
    /// Missing-frame report (gap list).
    Nack,
    /// Reconnection progress handshake (receiver → holder).
    Resume,
    /// Reconnection progress answer (holder → receiver).
    ResumeResponse,
    /// Connection liveness.
    Heartbeat,
    /// Protocol-level error.
    Error,
    /// Reserved for runtime session-security upgrade.
    SecurityUpgrade,
    /// Message-semantics layer JSON payload.
    DataJson,
    /// Raw bytes (multimodal content).
    DataBinary,
    /// Unknown type from a newer minor version; receivers ignore it (§8).
    Unknown(u16),
}

impl FrameType {
    /// Wire u16 value.
    pub fn to_u16(self) -> u16 {
        match self {
            Self::Negotiate => 0x0001,
            Self::Ack => 0x0002,
            Self::Nack => 0x0003,
            Self::Resume => 0x0004,
            Self::ResumeResponse => 0x0008,
            Self::Heartbeat => 0x0005,
            Self::Error => 0x0006,
            Self::SecurityUpgrade => 0x0007,
            Self::DataJson => 0x1001,
            Self::DataBinary => 0x1002,
            Self::Unknown(v) => v,
        }
    }

    /// Classify a wire u16 value; unrecognized values yield
    /// [`FrameType::Unknown`] (tolerated and ignorable per SPEC §8).
    pub fn from_u16(v: u16) -> Self {
        match v {
            0x0001 => Self::Negotiate,
            0x0002 => Self::Ack,
            0x0003 => Self::Nack,
            0x0004 => Self::Resume,
            0x0008 => Self::ResumeResponse,
            0x0005 => Self::Heartbeat,
            0x0006 => Self::Error,
            0x0007 => Self::SecurityUpgrade,
            0x1001 => Self::DataJson,
            0x1002 => Self::DataBinary,
            other => Self::Unknown(other),
        }
    }

    /// Max payload length for this frame class (§4); `None` for unknown
    /// types (they are instead bounded by the decode cap
    /// [`MAX_DECODE_TOTAL_CAP`]).
    pub fn max_frame_len(self) -> Option<usize> {
        match self {
            Self::Negotiate
            | Self::Ack
            | Self::Nack
            | Self::Resume
            | Self::ResumeResponse
            | Self::Heartbeat
            | Self::Error
            | Self::SecurityUpgrade => Some(MAX_CONTROL_FRAME_LEN),
            Self::DataJson => Some(MAX_DATA_JSON_FRAME_LEN),
            Self::DataBinary => Some(MAX_DATA_BINARY_FRAME_LEN),
            Self::Unknown(_) => None,
        }
    }
}

/// One binary wire frame (§3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Frame role.
    pub frame_type: FrameType,
    /// UUID of the owning transaction (16 raw bytes); resume anchor.
    pub transaction_id: [u8; 16],
    /// Total transaction byte count (reassembly criterion lives upper).
    pub total_len: u32,
    /// Frame sequence within the transaction, starting at 0.
    pub seq: u32,
    /// Payload bytes (`frame_len` on the wire).
    pub payload: Vec<u8>,
}

impl Frame {
    /// Assemble a frame.
    pub fn new(
        frame_type: FrameType,
        transaction_id: [u8; 16],
        total_len: u32,
        seq: u32,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            frame_type,
            transaction_id,
            total_len,
            seq,
            payload,
        }
    }

    /// Encode the full frame (header + payload + CRC trailer).
    ///
    /// Rejects payloads exceeding the per-type frame size limit (§4) with
    /// [`FrameError::FrameTooLarge`].
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        let max = self
            .frame_type
            .max_frame_len()
            .unwrap_or(MAX_DECODE_TOTAL_CAP - FRAME_OVERHEAD);
        if self.payload.len() > max {
            return Err(FrameError::FrameTooLarge {
                frame_type: self.frame_type,
                max,
                actual: self.payload.len(),
            });
        }
        let mut out = Vec::with_capacity(FRAME_OVERHEAD + self.payload.len());
        write_header(
            &mut out,
            &self.frame_type,
            &self.transaction_id,
            self.total_len,
            self.seq,
            self.payload.len(),
        );
        out.extend_from_slice(&self.payload);
        let crc = crc32(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        Ok(out)
    }
}

/// Standard CRC-32/ISO-HDLC (reflected polynomial `0xEDB88320`, IEEE/zlib
/// compatible). Interop anchor: `crc32(b"123456789") == 0xCBF43926`.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

/// Write the 36-byte fixed header (§3), big-endian.
fn write_header(
    out: &mut Vec<u8>,
    frame_type: &FrameType,
    transaction_id: &[u8; 16],
    total_len: u32,
    seq: u32,
    frame_len: usize,
) {
    out.extend_from_slice(&MAGIC);
    out.push(VERSION_MAJOR);
    out.push(VERSION_MINOR);
    out.extend_from_slice(&frame_type.to_u16().to_be_bytes());
    out.extend_from_slice(transaction_id);
    out.extend_from_slice(&total_len.to_be_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    // Callers bound frame_len by the per-type limits (≤ 16 MiB), so the
    // cast is lossless.
    out.extend_from_slice(&(frame_len as u32).to_be_bytes());
}

/// Frame codec error.
///
/// Fatal variants are connection-level (SPEC §7: the CRC is the primary
/// integrity defense in plaintext mode): once reported, the decoder must be
/// discarded together with the stream state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// Peer speaks an incompatible frame-format major (SPEC §8: refuse).
    #[error("version major mismatch: got {got}, expected {expected}")]
    VersionMismatch { got: u8, expected: u8 },
    /// Frame size exceeds the per-type limit (§4 protocol error).
    #[error("frame too large for {frame_type:?}: max {max}, got {actual}")]
    FrameTooLarge {
        frame_type: FrameType,
        max: usize,
        actual: usize,
    },
    /// CRC-32 mismatch over header + payload.
    #[error("crc mismatch: expected {expected:#010x}, got {actual:#010x}")]
    CrcMismatch { expected: u32, actual: u32 },
}

/// Outcome of validating a fully buffered 36-byte header.
struct HeaderInfo {
    frame_type: FrameType,
    transaction_id: [u8; 16],
    total_len: u32,
    seq: u32,
    frame_len: usize,
}

/// Validate a fully buffered header (`hdr.len() == HEADER_LEN`; magic is
/// already verified by the caller's scan).
///
/// - `version_major` mismatch is fatal ([`FrameError::VersionMismatch`]).
/// - A known type exceeding its §4 limit is a protocol error
///   ([`FrameError::FrameTooLarge`]) — not silently resynced past.
/// - An unknown type whose total size (header + frame_len + trailer)
///   exceeds the decode buffer cap is a protocol error too (§3 Rules:
///   regardless of frame type).
fn check_header(hdr: &[u8]) -> Result<HeaderInfo, FrameError> {
    let major = hdr[4];
    if major != VERSION_MAJOR {
        return Err(FrameError::VersionMismatch {
            got: major,
            expected: VERSION_MAJOR,
        });
    }
    let frame_type = FrameType::from_u16(u16::from_be_bytes([hdr[6], hdr[7]]));
    let frame_len = u32::from_be_bytes([hdr[32], hdr[33], hdr[34], hdr[35]]) as usize;
    if let Some(max) = frame_type.max_frame_len() {
        if frame_len > max {
            return Err(FrameError::FrameTooLarge {
                frame_type,
                max,
                actual: frame_len,
            });
        }
    } else if FRAME_OVERHEAD + frame_len > MAX_DECODE_TOTAL_CAP {
        return Err(FrameError::FrameTooLarge {
            frame_type,
            max: MAX_DECODE_TOTAL_CAP - FRAME_OVERHEAD,
            actual: frame_len,
        });
    }
    Ok(HeaderInfo {
        frame_type,
        transaction_id: hdr[8..24].try_into().expect("16-byte slice"),
        total_len: u32::from_be_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]),
        seq: u32::from_be_bytes([hdr[28], hdr[29], hdr[30], hdr[31]]),
        frame_len,
    })
}

/// Decoder internal state.
#[derive(Default)]
enum State {
    /// Scanning for a magic candidate (stream start or dirty-stream recovery).
    #[default]
    Sync,
    /// Header validated; accumulating payload + CRC trailer.
    Body {
        /// Kept for the CRC computation (covers header + payload).
        header: [u8; HEADER_LEN],
        frame_type: FrameType,
        transaction_id: [u8; 16],
        total_len: u32,
        seq: u32,
        frame_len: usize,
    },
}

/// Incremental frame decoder for byte streams (TCP / stdio).
///
/// Feed arbitrary chunks via [`feed`](Self::feed); complete frames come out
/// as a `Vec<Frame>`. Handles coalesced frames, frames split across chunks,
/// and dirty streams (resynchronization by magic scan + header validation;
/// pseudo-magic inside a frame payload never disturbs an in-flight frame).
///
/// Buffers are never pre-allocated from declared lengths: bodies accumulate
/// only after header validation caps them at the §4 limits.
///
/// After a fatal [`FrameError`] the decoder state is drained; discard the
/// decoder together with the connection.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    state: State,
}

impl FrameDecoder {
    /// Create a decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume a chunk of stream bytes; returns every frame completed by
    /// this chunk (possibly none, possibly several).
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, FrameError> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            if !self.progress(&mut frames)? {
                break;
            }
        }
        Ok(frames)
    }

    /// Attempt one state-machine step. `Ok(false)` = needs more input.
    fn progress(&mut self, frames: &mut Vec<Frame>) -> Result<bool, FrameError> {
        match &mut self.state {
            State::Sync => self.progress_sync(),
            State::Body { .. } => self.progress_body(frames),
        }
    }

    /// Sync state: find a magic candidate, validate its header.
    fn progress_sync(&mut self) -> Result<bool, FrameError> {
        let Some(pos) = self.buf.windows(4).position(|w| w == MAGIC) else {
            // No full magic in the buffer: keep only the trailing ≤3 bytes
            // (a possibly split magic prefix), drop the rest.
            let keep = self.buf.len().min(MAGIC.len() - 1);
            let drop_to = self.buf.len() - keep;
            self.buf.drain(..drop_to);
            return Ok(false); // need more input
        };
        self.buf.drain(..pos);
        if self.buf.len() < HEADER_LEN {
            return Ok(false); // candidate kept, waiting for the full header
        }
        let info = match check_header(&self.buf[..HEADER_LEN]) {
            Ok(info) => info,
            // Fatal: drain state so the discarded decoder cannot replay the
            // rejected header on subsequent feeds.
            Err(e) => {
                self.buf.clear();
                self.state = State::Sync;
                return Err(e);
            }
        };
        let mut header = [0u8; HEADER_LEN];
        header.copy_from_slice(&self.buf[..HEADER_LEN]);
        self.buf.drain(..HEADER_LEN);
        self.state = State::Body {
            header,
            frame_type: info.frame_type,
            transaction_id: info.transaction_id,
            total_len: info.total_len,
            seq: info.seq,
            frame_len: info.frame_len,
        };
        Ok(true)
    }

    /// Body state: accumulate payload + CRC, verify, emit the frame.
    fn progress_body(&mut self, frames: &mut Vec<Frame>) -> Result<bool, FrameError> {
        let State::Body {
            header,
            frame_type,
            transaction_id,
            total_len,
            seq,
            frame_len,
        } = &mut self.state
        else {
            unreachable!("guarded by progress()")
        };
        let need = *frame_len + CRC_LEN;
        if self.buf.len() < need {
            return Ok(false); // body incomplete, waiting
        }
        let mut hasher = Hasher::new();
        hasher.update(header);
        hasher.update(&self.buf[..*frame_len]);
        let actual = hasher.finalize();
        let expected = u32::from_be_bytes([
            self.buf[*frame_len],
            self.buf[*frame_len + 1],
            self.buf[*frame_len + 2],
            self.buf[*frame_len + 3],
        ]);
        if actual != expected {
            self.buf.clear();
            self.state = State::Sync;
            return Err(FrameError::CrcMismatch { expected, actual });
        }
        let frame = Frame {
            frame_type: *frame_type,
            transaction_id: *transaction_id,
            total_len: *total_len,
            seq: *seq,
            payload: self.buf[..*frame_len].to_vec(),
        };
        self.buf.drain(..need);
        self.state = State::Sync;
        frames.push(frame);
        Ok(true)
    }
}
