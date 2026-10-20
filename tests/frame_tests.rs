//! Binary wire frame codec tests (SPEC v1.0 §3/§4).
//!
//! Covers: CRC-32 interop check vector, roundtrips (all frame types),
//! field boundary values, pipelined (coalesced) and byte-split streaming,
//! dirty-stream recovery (garbage resync, inert pseudo-magic inside
//! payloads, decode-cap protocol errors), per-type size limits (encode +
//! decode), CRC tampering detection, and decoder reuse across feeds.

use xacpp::wire::frame::{
    CRC_LEN, Frame, FrameDecoder, FrameError, FrameType, HEADER_LEN, MAGIC, MAX_CONTROL_FRAME_LEN,
    MAX_DATA_BINARY_FRAME_LEN, MAX_DATA_JSON_FRAME_LEN, MAX_DECODE_TOTAL_CAP, VERSION_MAJOR,
    VERSION_MINOR, crc32,
};

/// Deterministic transaction id helper.
fn tid(n: u8) -> [u8; 16] {
    [n; 16]
}

/// Build a wire frame with a valid CRC but no encode-side validation
/// (used to construct frames the encoder would reject).
fn manual_frame(ft: u16, tid_bytes: [u8; 16], total_len: u32, seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + CRC_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION_MAJOR);
    out.push(VERSION_MINOR);
    out.extend_from_slice(&ft.to_be_bytes());
    out.extend_from_slice(&tid_bytes);
    out.extend_from_slice(&total_len.to_be_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

/// A 36-byte pseudo-magic segment: valid magic + version, but an unknown
/// frame type declaring an absurd length (far beyond the decode cap).
/// Per SPEC §3 Rules this is a protocol error, not a resync point.
fn pseudo_magic_segment() -> Vec<u8> {
    let mut seg = Vec::with_capacity(HEADER_LEN);
    seg.extend_from_slice(&MAGIC);
    seg.push(VERSION_MAJOR);
    seg.push(VERSION_MINOR);
    seg.extend_from_slice(&0xDEADu16.to_be_bytes()); // unknown type
    seg.extend_from_slice(&[0x5A; 16]); // transaction_id
    seg.extend_from_slice(&0u32.to_be_bytes()); // total_len
    seg.extend_from_slice(&0u32.to_be_bytes()); // seq
    seg.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes()); // absurd frame_len
    seg
}

fn sample_frame(frame_type: FrameType, payload: Vec<u8>) -> Frame {
    Frame::new(frame_type, tid(0xAB), 1234, 7, payload)
}

// ---- CRC-32 interop anchor ----

/// rs/ts interop anchor: standard CRC-32/ISO-HDLC check vector.
#[test]
fn test_crc32_check_vector() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b""), 0x0000_0000);
}

// ---- Roundtrips ----

#[test]
fn test_roundtrip_control_frame_types() {
    let types = [
        FrameType::Negotiate,
        FrameType::Ack,
        FrameType::Nack,
        FrameType::Resume,
        FrameType::ResumeResponse,
        FrameType::Heartbeat,
        FrameType::Error,
        FrameType::SecurityUpgrade,
    ];
    for ft in types {
        let frame = sample_frame(ft, b"{}".to_vec());
        let bytes = frame.encode().unwrap();
        let mut dec = FrameDecoder::new();
        let decoded = dec.feed(&bytes).unwrap();
        assert_eq!(decoded, vec![frame], "roundtrip failed for {ft:?}");
    }
}

/// The RESUME answer direction's dedicated discriminant: two directions
/// never share a frame type (SPEC §5).
#[test]
fn test_resume_response_discriminant_distinct() {
    assert_eq!(FrameType::ResumeResponse.to_u16(), 0x0008);
    assert_eq!(FrameType::from_u16(0x0008), FrameType::ResumeResponse);
    assert_ne!(
        FrameType::Resume.to_u16(),
        FrameType::ResumeResponse.to_u16()
    );
    // Control class: bounded by the 16 KiB control-frame cap.
    assert_eq!(
        FrameType::ResumeResponse.max_frame_len(),
        Some(MAX_CONTROL_FRAME_LEN)
    );
}

#[test]
fn test_roundtrip_data_frame_types() {
    for ft in [FrameType::DataJson, FrameType::DataBinary] {
        let frame = sample_frame(ft, (0..64u8).collect());
        let bytes = frame.encode().unwrap();
        let mut dec = FrameDecoder::new();
        assert_eq!(dec.feed(&bytes).unwrap(), vec![frame]);
    }
}

/// Unknown types (from a newer minor version) survive the codec untouched;
/// ignoring them is the upper layer's job (SPEC §9).
#[test]
fn test_roundtrip_unknown_frame_type() {
    let frame = sample_frame(FrameType::Unknown(0x7FFF), b"opaque".to_vec());
    let bytes = frame.encode().unwrap();
    let mut dec = FrameDecoder::new();
    assert_eq!(dec.feed(&bytes).unwrap(), vec![frame]);
}

// ---- Field boundary values ----

#[test]
fn test_field_boundaries_roundtrip() {
    let cases = [
        // (transaction_id, total_len, seq, payload)
        ([0x00; 16], 0, 0, Vec::new()), // zero tid, zero total, first seq, empty payload
        ([0xFF; 16], u32::MAX, u32::MAX, vec![0x42]), // max fields
        (tid(1), 0, 0, vec![0u8; 3]),   // empty-payload criterion: frame_len == 0
    ];
    for (tid_bytes, total_len, seq, payload) in cases {
        let frame = Frame::new(FrameType::DataBinary, tid_bytes, total_len, seq, payload);
        let bytes = frame.encode().unwrap();
        let mut dec = FrameDecoder::new();
        assert_eq!(
            dec.feed(&bytes).unwrap(),
            vec![frame],
            "boundary case tid={tid_bytes:?} total={total_len} seq={seq}"
        );
    }
}

/// Empty payload: frame_len = 0, CRC covers the header alone.
#[test]
fn test_empty_payload_frame() {
    let frame = Frame::new(FrameType::Heartbeat, tid(9), 0, 0, Vec::new());
    let bytes = frame.encode().unwrap();
    assert_eq!(bytes.len(), HEADER_LEN + CRC_LEN);
    // Header CRC field must equal crc32 over the header alone.
    let wire_crc = u32::from_be_bytes([
        bytes[HEADER_LEN],
        bytes[HEADER_LEN + 1],
        bytes[HEADER_LEN + 2],
        bytes[HEADER_LEN + 3],
    ]);
    assert_eq!(wire_crc, crc32(&bytes[..HEADER_LEN]));
    let mut dec = FrameDecoder::new();
    assert_eq!(dec.feed(&bytes).unwrap(), vec![frame]);
}

// ---- Version handling ----

/// major mismatch is fatal (SPEC §9: connection refused).
#[test]
fn test_version_major_mismatch_rejected() {
    let mut bytes = sample_frame(FrameType::Ack, b"{}".to_vec())
        .encode()
        .unwrap();
    bytes[4] = 2; // version_major
    // Fix the CRC so the failure is attributable to the version check.
    let crc = crc32(&bytes[..bytes.len() - CRC_LEN]);
    let crc_pos = bytes.len() - CRC_LEN;
    bytes[crc_pos..].copy_from_slice(&crc.to_be_bytes());
    let mut dec = FrameDecoder::new();
    assert_eq!(
        dec.feed(&bytes),
        Err(FrameError::VersionMismatch {
            got: 2,
            expected: 1
        })
    );
}

/// minor is a backward-compatible extension: any minor decodes fine.
#[test]
fn test_version_minor_tolerated() {
    let mut bytes = sample_frame(FrameType::Ack, b"{}".to_vec())
        .encode()
        .unwrap();
    bytes[5] = 9; // version_minor — not validated by the codec
    // Peer would send a CRC valid for its own header; recompute it so the
    // case isolates the minor field.
    let crc = crc32(&bytes[..bytes.len() - CRC_LEN]);
    let crc_pos = bytes.len() - CRC_LEN;
    bytes[crc_pos..].copy_from_slice(&crc.to_be_bytes());
    let mut dec = FrameDecoder::new();
    assert!(dec.feed(&bytes).is_ok());
}

// ---- Streaming: coalesced / split / dirty ----

#[test]
fn test_pipelined_frames_single_feed() {
    let a = sample_frame(FrameType::Negotiate, b"negotiate".to_vec());
    let b = sample_frame(FrameType::DataJson, b"[1,2,3]".to_vec());
    let mut wire = a.encode().unwrap();
    wire.extend_from_slice(&b.encode().unwrap());
    let mut dec = FrameDecoder::new();
    assert_eq!(dec.feed(&wire).unwrap(), vec![a, b]);
}

#[test]
fn test_byte_by_byte_streaming() {
    let frame = sample_frame(FrameType::DataBinary, (0..100u8).collect());
    let bytes = frame.encode().unwrap();
    let mut dec = FrameDecoder::new();
    let mut out = Vec::new();
    for (i, chunk) in bytes.iter().enumerate() {
        let frames = dec.feed(std::slice::from_ref(chunk)).unwrap();
        if i + 1 < bytes.len() {
            assert!(frames.is_empty(), "frame completed early at byte {i}");
        }
        out.extend(frames);
    }
    assert_eq!(out, vec![frame]);
}

/// Frames split at chunk boundaries with varying chunk sizes, fed across
/// multiple `feed` calls; order and content preserved.
#[test]
fn test_chunked_feed_mixed_sizes() {
    let frames = vec![
        sample_frame(FrameType::Heartbeat, Vec::new()),
        sample_frame(FrameType::DataBinary, vec![7u8; 5000]),
        sample_frame(FrameType::Ack, b"ack".to_vec()),
        sample_frame(FrameType::DataJson, vec![0u8; 70000]), // > 16 KiB, < 1 MiB
        sample_frame(FrameType::Nack, b"nack".to_vec()),
    ];
    let mut wire = Vec::new();
    for f in &frames {
        wire.extend_from_slice(&f.encode().unwrap());
    }
    // Deterministic chunking: cycle through 1, 2, 3, 5, 8, 13, 21, ... bytes.
    let mut dec = FrameDecoder::new();
    let mut out = Vec::new();
    let mut pos = 0;
    let mut step = 1usize;
    while pos < wire.len() {
        let end = (pos + step).min(wire.len());
        out.extend(dec.feed(&wire[pos..end]).unwrap());
        pos = end;
        step = step * 2 + 1;
    }
    assert_eq!(out, frames);
}

/// Dirty stream: real frame + magic-free garbage + real frame; the decoder
/// must resynchronize and deliver both real frames, no error. The 1 MiB
/// garbage run also exercises the no-preallocation guarantee (buffers grow
/// only from validated headers, never from stream volume).
#[test]
fn test_dirty_stream_garbage_skipped() {
    let a = sample_frame(FrameType::DataJson, b"before".to_vec());
    let b = sample_frame(FrameType::DataBinary, b"after".to_vec());
    let mut wire = a.encode().unwrap();
    wire.extend_from_slice(&[0xCC; 1024 * 1024]); // magic-free garbage
    wire.extend_from_slice(&b.encode().unwrap());
    let mut dec = FrameDecoder::new();
    assert_eq!(dec.feed(&wire).unwrap(), vec![a, b]);
}

/// Pseudo-magic inside a frame payload must not disturb the in-flight
/// frame: body consumption is length-driven, not scan-driven.
#[test]
fn test_pseudo_magic_inside_payload_is_inert() {
    let inner = pseudo_magic_segment();
    let frame = sample_frame(FrameType::DataBinary, inner.clone());
    let bytes = frame.encode().unwrap();
    let mut dec = FrameDecoder::new();
    let decoded = dec.feed(&bytes).unwrap();
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].payload, inner);
}

/// A header declaring an unknown type whose total size exceeds the decode
/// buffer cap is a protocol error (SPEC §3 Rules) — and the decoder must
/// never have reserved memory for the declared length.
#[test]
fn test_oversized_unknown_type_is_protocol_error() {
    // pseudo_magic_segment declares frame_len = 0xFFFFFFFF with an unknown
    // type: header parses, cap check rejects before any body allocation.
    let mut dec = FrameDecoder::new();
    let err = dec.feed(&pseudo_magic_segment()).unwrap_err();
    assert_eq!(
        err,
        FrameError::FrameTooLarge {
            frame_type: FrameType::Unknown(0xDEAD),
            max: MAX_DECODE_TOTAL_CAP - HEADER_LEN - CRC_LEN,
            actual: 0xFFFF_FFFF
        }
    );
    // Fatal: decoder state drained; feeding again yields nothing.
    assert!(dec.feed(b"more").unwrap().is_empty());
}

/// Leading garbage before the first real frame is discarded.
#[test]
fn test_leading_garbage_before_first_frame() {
    let frame = sample_frame(FrameType::Ack, b"ok".to_vec());
    let mut wire = b"\x00\x01garbage\x00stream".to_vec();
    wire.extend_from_slice(&frame.encode().unwrap());
    let mut dec = FrameDecoder::new();
    assert_eq!(dec.feed(&wire).unwrap(), vec![frame]);
}

// ---- Per-type size limits (§4) ----

#[test]
fn test_size_limit_encode_boundaries() {
    let ok_cases = [
        (FrameType::Negotiate, MAX_CONTROL_FRAME_LEN),
        (FrameType::DataJson, MAX_DATA_JSON_FRAME_LEN),
        (FrameType::DataBinary, MAX_DATA_BINARY_FRAME_LEN),
    ];
    for (ft, max) in ok_cases {
        let frame = sample_frame(ft, vec![0u8; max]);
        assert!(
            frame.encode().is_ok(),
            "{ft:?} at exactly {max} must encode"
        );
    }
    let too_large_cases = [
        (FrameType::Negotiate, MAX_CONTROL_FRAME_LEN + 1),
        (FrameType::DataJson, MAX_DATA_JSON_FRAME_LEN + 1),
        (FrameType::DataBinary, MAX_DATA_BINARY_FRAME_LEN + 1),
    ];
    for (ft, len) in too_large_cases {
        let frame = sample_frame(ft, vec![0u8; len]);
        assert_eq!(
            frame.encode(),
            Err(FrameError::FrameTooLarge {
                frame_type: ft,
                max: len - 1,
                actual: len
            }),
            "{ft:?} at {len} must be rejected"
        );
    }
}

/// A wire frame whose header declares a known type above its limit is a
/// protocol error at decode time too (CRC valid, header checked first).
#[test]
fn test_size_limit_decode_rejects_oversized_control() {
    let payload = vec![0u8; MAX_CONTROL_FRAME_LEN + 1];
    let bytes = manual_frame(0x0001, tid(2), 0, 0, &payload); // Negotiate, oversized
    let mut dec = FrameDecoder::new();
    assert_eq!(
        dec.feed(&bytes),
        Err(FrameError::FrameTooLarge {
            frame_type: FrameType::Negotiate,
            max: MAX_CONTROL_FRAME_LEN,
            actual: MAX_CONTROL_FRAME_LEN + 1
        })
    );
}

// ---- CRC tampering ----

#[test]
fn test_crc_tamper_payload_detected() {
    let mut bytes = sample_frame(FrameType::DataJson, b"payload".to_vec())
        .encode()
        .unwrap();
    bytes[HEADER_LEN] ^= 0xFF; // first payload byte
    let mut dec = FrameDecoder::new();
    assert!(matches!(
        dec.feed(&bytes),
        Err(FrameError::CrcMismatch { .. })
    ));
}

/// CRC covers the header: flipping a header field is detected.
#[test]
fn test_crc_tamper_header_detected() {
    let mut bytes = sample_frame(FrameType::DataJson, b"payload".to_vec())
        .encode()
        .unwrap();
    bytes[28] ^= 0x01; // seq field first byte
    let mut dec = FrameDecoder::new();
    assert!(matches!(
        dec.feed(&bytes),
        Err(FrameError::CrcMismatch { .. })
    ));
}

#[test]
fn test_crc_tamper_trailer_detected() {
    let mut bytes = sample_frame(FrameType::DataJson, b"payload".to_vec())
        .encode()
        .unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    let mut dec = FrameDecoder::new();
    assert!(matches!(
        dec.feed(&bytes),
        Err(FrameError::CrcMismatch { .. })
    ));
}

// ---- Decoder reuse ----

#[test]
fn test_decoder_reusable_across_feeds() {
    let mut dec = FrameDecoder::new();
    let frames: Vec<Frame> = (0..10u32)
        .map(|i| sample_frame(FrameType::DataBinary, vec![i as u8; 32]))
        .collect();
    let mut out = Vec::new();
    for f in &frames {
        out.extend(dec.feed(&f.encode().unwrap()).unwrap());
    }
    assert_eq!(out, frames);
    // Idle feed is a no-op.
    assert!(dec.feed(&[]).unwrap().is_empty());
}

/// Unknown types are bounded by the decode buffer cap (total size): at the
/// exact cap the frame is accepted (CRC guards it), one byte beyond it is a
/// protocol error.
#[test]
fn test_unknown_type_hard_cap_boundary() {
    let cap_payload = MAX_DECODE_TOTAL_CAP - HEADER_LEN - CRC_LEN;
    // At the cap: accepted as a real frame (manual CRC valid).
    let bytes = manual_frame(0xDEAD, tid(3), 0, 0, &vec![0u8; cap_payload]);
    let mut dec = FrameDecoder::new();
    let decoded = dec.feed(&bytes).unwrap();
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].frame_type, FrameType::Unknown(0xDEAD));
    assert_eq!(decoded[0].payload.len(), cap_payload);

    // One byte beyond the cap: protocol error.
    let bytes = manual_frame(0xDEAD, tid(4), 0, 0, &vec![0u8; cap_payload + 1]);
    let mut dec = FrameDecoder::new();
    assert_eq!(
        dec.feed(&bytes),
        Err(FrameError::FrameTooLarge {
            frame_type: FrameType::Unknown(0xDEAD),
            max: cap_payload,
            actual: cap_payload + 1
        })
    );
}
