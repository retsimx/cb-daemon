//! Frame encode/decode and scanner for Android Auto CB bursts.
//!
//! Wire form: `<U>{payload}</U={crc:02x}>` with lowercase hex CRC-8 over
//! payload bytes only. Multiple frames in a CB burst are separated by ASCII
//! space (`0x20`).

use aa_crc::crc8;

const PREFIX: &[u8] = b"<U>";
const MID: &[u8] = b"</U=";
const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

/// A single CB protocol frame carrying an opaque payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Raw payload bytes (CRC is computed over these alone).
    pub payload: Vec<u8>,
}

impl Frame {
    /// Encode as `<U>{payload}</U={crc:02x}>` with lowercase hex CRC of payload bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let crc = crc8(&self.payload);
        let mut out = Vec::with_capacity(PREFIX.len() + self.payload.len() + MID.len() + 3);
        out.extend_from_slice(PREFIX);
        out.extend_from_slice(&self.payload);
        out.extend_from_slice(MID);
        out.push(HEX_LOWER[usize::from(crc >> 4)]);
        out.push(HEX_LOWER[usize::from(crc & 0x0f)]);
        out.push(b'>');
        out
    }

    /// Parse one frame from the start of `input` (skipping leading spaces).
    ///
    /// Returns `(frame, bytes_consumed)` where `bytes_consumed` includes any
    /// leading spaces that were skipped plus the full frame bytes.
    ///
    /// # Errors
    ///
    /// - [`FrameError::Incomplete`] if `input` does not yet contain a full frame
    /// - [`FrameError::InvalidCrc`] if the trailer CRC does not match the payload
    /// - [`FrameError::Malformed`] if the framing tokens or hex digits are invalid
    pub fn decode_one(input: &[u8]) -> Result<(Self, usize), FrameError> {
        let leading = input.iter().take_while(|&&b| b == b' ').count();
        let rest = &input[leading..];

        if rest.is_empty() {
            return Err(FrameError::Incomplete);
        }

        if rest.len() < PREFIX.len() {
            return Err(FrameError::Incomplete);
        }
        if !rest.starts_with(PREFIX) {
            return Err(FrameError::Malformed);
        }

        let after_prefix = &rest[PREFIX.len()..];
        let Some(mid_at) = find_slice(after_prefix, MID) else {
            return Err(FrameError::Incomplete);
        };

        let payload = &after_prefix[..mid_at];
        let after_mid = &after_prefix[mid_at + MID.len()..];

        if after_mid.len() < 3 {
            return Err(FrameError::Incomplete);
        }

        let expected = parse_hex_byte(after_mid[0], after_mid[1])?;
        if after_mid[2] != b'>' {
            return Err(FrameError::Malformed);
        }

        let actual = crc8(payload);
        if actual != expected {
            return Err(FrameError::InvalidCrc { expected, actual });
        }

        let frame_len = PREFIX.len() + payload.len() + MID.len() + 3;
        let consumed = leading + frame_len;
        Ok((
            Self {
                payload: payload.to_vec(),
            },
            consumed,
        ))
    }
}

/// Incremental scanner that reassembles frames across partial `push` chunks.
///
/// Incomplete trailing bytes are retained in an internal buffer with **no size
/// cap**. Pure leading spaces are drained so idle gaps do not grow forever, but
/// a never-completing frame (or untrusted stream) can still grow without bound.
/// Callers that later feed network/serial I/O should impose an upper limit
/// before `push` (out of scope for this foundation crate).
#[derive(Debug, Default, Clone)]
pub struct FrameScanner {
    buf: Vec<u8>,
}

impl FrameScanner {
    /// Create an empty scanner.
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append `chunk` and extract every complete frame available.
    ///
    /// Incomplete trailing data stays buffered (unbounded — see type docs).
    /// Leading/trailing spaces between frames are accepted. Soft incompleteness
    /// yields an empty (or partial) `Ok` list rather than [`FrameError::Incomplete`].
    ///
    /// On [`FrameError::Malformed`] / [`FrameError::InvalidCrc`], the scanner
    /// **resyncs** to the next `<U>` (or keeps a short partial-prefix tail) and
    /// continues — it does not wedge on leading AOA/bus garbage. Those errors
    /// are returned only when no frames were extracted **and** resync discarded
    /// bytes with no subsequent `<U>` in this push (caller may log a warn).
    ///
    /// # Errors
    ///
    /// May return [`FrameError::InvalidCrc`] or [`FrameError::Malformed`] when
    /// a push discarded noise and produced no frames (resync with no recovery
    /// target yet). Incomplete data alone is never an error from `push`.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, FrameError> {
        self.buf.extend_from_slice(chunk);

        let mut frames = Vec::new();
        let mut last_resync_err: Option<FrameError> = None;
        loop {
            // Drop pure leading spaces so an idle trailing gap does not grow forever.
            let leading = self.buf.iter().take_while(|&&b| b == b' ').count();
            if leading == self.buf.len() {
                self.buf.clear();
                break;
            }
            if leading > 0 {
                self.buf.drain(..leading);
            }

            match Frame::decode_one(&self.buf) {
                Ok((frame, consumed)) => {
                    self.buf.drain(..consumed);
                    frames.push(frame);
                    last_resync_err = None;
                }
                Err(FrameError::Incomplete) => break,
                Err(e) => {
                    // Skip current byte / bad frame start and hunt for the next PREFIX.
                    // Keeping a short tail preserves a split `<U` across chunks.
                    if let Some(rel) = find_slice(&self.buf[1..], PREFIX) {
                        self.buf.drain(..=rel);
                        last_resync_err = Some(e);
                    } else {
                        let keep = PREFIX.len().saturating_sub(1).min(self.buf.len());
                        let drop = self.buf.len() - keep;
                        if drop > 0 {
                            self.buf.drain(..drop);
                            last_resync_err = Some(e);
                        }
                        break;
                    }
                }
            }
        }
        if frames.is_empty()
            && let Some(err) = last_resync_err
        {
            return Err(err);
        }
        Ok(frames)
    }
}

/// Errors produced while decoding or scanning frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// Input ended before a complete frame was available.
    Incomplete,
    /// Trailer CRC (`expected`) does not match CRC computed over the payload (`actual`).
    InvalidCrc {
        /// CRC value parsed from the frame trailer.
        expected: u8,
        /// CRC computed over the payload bytes.
        actual: u8,
    },
    /// Framing tokens or hex digits are not well-formed.
    Malformed,
}

fn find_slice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_hex_byte(hi: u8, lo: u8) -> Result<u8, FrameError> {
    Ok((hex_nibble(hi)? << 4) | hex_nibble(lo)?)
}

const fn hex_nibble(b: u8) -> Result<u8, FrameError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(FrameError::Malformed),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn round_trip(payload: &[u8]) {
        let frame = Frame {
            payload: payload.to_vec(),
        };
        let encoded = frame.encode();
        let (decoded, consumed) = Frame::decode_one(&encoded).expect("decode");
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn round_trip_ping() {
        round_trip(b"Ping");
    }

    #[test]
    fn round_trip_set_can() {
        round_trip(b"setCAN ");
    }

    #[test]
    fn round_trip_ack_can() {
        round_trip(b"ackCAN 1");
    }

    #[test]
    fn encode_ping_golden_wire() {
        let frame = Frame {
            payload: b"Ping".to_vec(),
        };
        assert_eq!(frame.encode(), b"<U>Ping</U=db>");
    }

    #[test]
    fn decode_skips_leading_spaces() {
        let input = b"  <U>Ping</U=db>";
        let (frame, consumed) = Frame::decode_one(input).unwrap();
        assert_eq!(frame.payload, b"Ping");
        assert_eq!(consumed, input.len());
    }

    #[test]
    fn scanner_splits_multiple_frames() {
        let mut scanner = FrameScanner::new();
        let burst = b"<U>Ping</U=db> <U>setCAN </U=b2> <U>ackCAN 1</U=aa>";
        let frames = scanner.push(burst).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].payload, b"Ping");
        assert_eq!(frames[1].payload, b"setCAN ");
        assert_eq!(frames[2].payload, b"ackCAN 1");
        // Trailing gap consumed; further push of empty yields nothing.
        assert!(scanner.push(&[]).unwrap().is_empty());
    }

    #[test]
    fn scanner_handles_partial_chunks() {
        let mut scanner = FrameScanner::new();
        let full = b"<U>Ping</U=db>";
        let mid = full.len() / 2;

        let first = scanner.push(&full[..mid]).unwrap();
        assert!(first.is_empty());

        let second = scanner.push(&full[mid..]).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].payload, b"Ping");
    }

    #[test]
    fn decode_incomplete() {
        let partial = b"<U>Ping</U=";
        assert_eq!(Frame::decode_one(partial), Err(FrameError::Incomplete));
        assert_eq!(Frame::decode_one(b""), Err(FrameError::Incomplete));
        assert_eq!(Frame::decode_one(b"   "), Err(FrameError::Incomplete));
    }

    #[test]
    fn scanner_incomplete_returns_empty() {
        let mut scanner = FrameScanner::new();
        let out = scanner.push(b"<U>Pi").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn bad_crc_invalid() {
        let bad = b"<U>Ping</U=00>";
        match Frame::decode_one(bad) {
            Err(FrameError::InvalidCrc { expected, actual }) => {
                assert_eq!(expected, 0x00);
                assert_eq!(actual, 0xdb);
            }
            other => panic!("expected InvalidCrc, got {other:?}"),
        }
    }

    #[test]
    fn malformed_prefix() {
        assert_eq!(
            Frame::decode_one(b"Ping</U=db>"),
            Err(FrameError::Malformed)
        );
        assert_eq!(
            Frame::decode_one(b"<X>Ping</U=db>"),
            Err(FrameError::Malformed)
        );
    }

    #[test]
    fn scanner_resyncs_past_leading_garbage_to_ping() {
        // Regression: AOA noise before `<U>` wedged the scanner on Malformed forever.
        let mut scanner = FrameScanner::new();
        let noise = b"\x00\xe1\x00\x00\x08\x01\x00\x00";
        let ping = b"<U>Ping</U=db>";
        let mut burst = noise.to_vec();
        burst.extend_from_slice(ping);
        let frames = scanner.push(&burst).expect("resync should yield ping");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, b"Ping");
    }

    #[test]
    fn scanner_resyncs_past_bad_crc_to_next_frame() {
        // Regression: bad CRC must consume the bad frame and continue, not wedge.
        let mut scanner = FrameScanner::new();
        let burst = b"<U>Ping</U=00><U>Ping</U=db>";
        let frames = scanner.push(burst).expect("bad crc should not wedge");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, b"Ping");
    }

    #[test]
    fn scanner_reassembles_large_getcan_in_aoa_chunks() {
        // Regression: live unit-08 dump replies are ~425-byte getCAN payloads.
        // AOA delivers them in ≤63-byte USB chunks; scanner must not Malformed-resync
        // mid-frame and destroy the payload.
        use std::fmt::Write;

        let mut records = String::from("getCAN 1");
        for i in 0..16 {
            records.push(' ');
            let _ = write!(records, "070311111050101033{i:02x}00100");
        }
        let frame = Frame {
            payload: records.as_bytes().to_vec(),
        };
        let encoded = frame.encode();
        assert!(encoded.len() > 200, "encoded len {}", encoded.len());

        let mut scanner = FrameScanner::new();
        let chunk = 63usize;
        let mut got = Vec::new();
        for start in (0..encoded.len()).step_by(chunk) {
            let end = (start + chunk).min(encoded.len());
            let frames = scanner
                .push(&encoded[start..end])
                .expect("chunked large getCAN must not Malformed");
            got.extend(frames);
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].payload, records.as_bytes());
    }

    #[test]
    fn scanner_noise_only_returns_malformed_once() {
        let mut scanner = FrameScanner::new();
        let err = scanner
            .push(b"\x00\xe1\x00\x00")
            .expect_err("noise with no <U> should surface");
        assert_eq!(err, FrameError::Malformed);
        // Buffer kept a short prefix tail; further empty push is quiet.
        assert!(scanner.push(&[]).unwrap().is_empty());
    }
}
