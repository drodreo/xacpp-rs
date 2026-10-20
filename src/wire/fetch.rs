//! Fetch channel kernel (SPEC §8) — a symmetric, stateless pull convention.
//!
//! The kernel composes two existing primitives — one command round-trip and
//! one binary transaction — and introduces **no fetch-specific protocol
//! state** (SPEC §8): the requester sends `fetch{uri}`, the
//! responder's consumer handler (a `SessionHandler::on_command`
//! implementation — the Peer layer does not intercept fetch) answers
//! `fetch_ready{txid, sha256, sizeBytes, mime}` and streams the bytes as a
//! regular [`crate::transport::TransferSession`] transaction
//! (`open_binary_sink` with the announced txid). Everything afterwards —
//! buffering, persistence, integrity verification, progress accounting —
//! is an application-layer concern; optional conveniences (integrity
//! promises, pause/cancel bookkeeping) belong to extension modules built
//! ON TOP of the kernel, never inside it (SPEC §8.6 contract firewall).
//!
//! What lives here: the fetch error-code registry, the thin requester shell
//! ([`FetchReadyMeta`] — `XacppSession::fetch` sends the command and waits
//! for the metadata response; it never carries bytes), and the transaction
//! id wire helpers shared with the stream surface.

use crate::wire::error::XacppError;
use crate::wire::message::XacppResponse;

/// Fetch wire error codes (registry — identical across rs/ts).
pub mod error_codes {
    /// uri unknown or resource gone (handler-declared).
    pub const FETCH_NOT_FOUND: &str = "fetch_not_found";
    /// uri outside the servable scope, or pause/resume not implemented
    /// (handler-declared).
    pub const FETCH_REFUSED: &str = "fetch_refused";
    /// fetch unsupported on this end (no fetch responder loaded).
    pub const FETCH_UNAVAILABLE: &str = "fetch_unavailable";
}

/// Metadata announced by a `fetch_ready` response (SPEC §8.2): the txid IS
/// the binary transaction id of the upcoming stream; `sha256`/`size_bytes`
/// are responder-computed from the bytes it sends. Carrying bytes is not
/// part of the kernel — claim the stream via
/// [`crate::session::XacppSession::on_binary_stream`] and match
/// [`FetchReadyMeta::txid`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchReadyMeta {
    /// Binary transaction id of the upcoming stream (16 raw bytes; the wire
    /// form is 32 lowercase hex chars).
    pub txid: [u8; 16],
    /// sha256-hex (lowercase) computed by the responder.
    pub sha256: String,
    /// Payload size in bytes.
    pub size_bytes: u64,
    /// MIME type declared by the responder.
    pub mime: String,
}

/// Parses a `fetch_ready` transaction id. The wire form is exactly 32
/// lowercase hex chars — no other form is accepted (SPEC §8 wire contract).
pub fn parse_transaction_id(s: &str) -> Result<[u8; 16], XacppError> {
    if s.len() != 32 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(XacppError::Internal(format!(
            "malformed fetch transaction id '{s}': expected 32 lowercase hex chars"
        )));
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = (s.as_bytes()[i * 2] as char).to_digit(16).ok_or_else(|| {
            XacppError::Internal(format!(
                "malformed fetch transaction id '{s}': expected 32 lowercase hex chars"
            ))
        })? as u8;
        let lo = (s.as_bytes()[i * 2 + 1] as char)
            .to_digit(16)
            .ok_or_else(|| {
                XacppError::Internal(format!(
                    "malformed fetch transaction id '{s}': expected 32 lowercase hex chars"
                ))
            })? as u8;
        *byte = (hi << 4) | lo;
    }
    Ok(out)
}

/// Renders a binary transaction id in the wire form (32 lowercase hex
/// chars) — what a responder puts into `fetch_ready.txid`.
pub fn format_transaction_id(txid: [u8; 16]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(32);
    for b in txid {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Maps a fetch command response to its metadata or a fetch error
/// (shared by the requester shell in `XacppSession::fetch`).
pub(crate) fn response_to_meta(response: XacppResponse) -> Result<FetchReadyMeta, XacppError> {
    match response {
        XacppResponse::FetchReady {
            txid,
            sha256,
            size_bytes,
            mime,
        } => {
            let txid = parse_transaction_id(&txid)?;
            Ok(FetchReadyMeta {
                txid,
                sha256,
                size_bytes,
                mime,
            })
        }
        XacppResponse::Error { code, message } => Err(XacppError::Application { code, message }),
        other => Err(XacppError::Internal(format!(
            "unexpected response to fetch: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_id_roundtrip() {
        let mut id = [0u8; 16];
        id[15] = 7;
        let s = format_transaction_id(id);
        assert_eq!(s.len(), 32);
        assert_eq!(parse_transaction_id(&s).unwrap(), id);
        // The wire form is exactly 32 lowercase hex chars: hyphenated UUIDs,
        // uppercase hex, and arbitrary junk are all rejected.
        let hyphenated = uuid::Uuid::from_bytes(id).to_string();
        assert!(parse_transaction_id(&hyphenated).is_err());
        let mut lettered = [0u8; 16];
        lettered[0] = 0xab;
        lettered[15] = 0xef;
        assert!(parse_transaction_id(&format_transaction_id(lettered).to_uppercase()).is_err());
        assert!(parse_transaction_id("not-a-uuid").is_err());
        assert!(parse_transaction_id("").is_err());
    }
}
