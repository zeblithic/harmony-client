//! ZEB-572 — the single audited length-prefixed framing codec for iroh
//! bi-streams.
//!
//! A frame is `[u32 length prefix][body]`. Historically this codec was
//! open-coded at ~10 sites in two endiannesses, each re-deriving its own
//! cap-before-alloc DoS guard. This module is the one audited implementation.
//!
//! Two layers:
//!  * [`encode_len_prefix`] / [`decode_len_prefix`] — the pure, sync security
//!    boundary (the cap-before-alloc guard). Every site uses these for the
//!    length check, including sites that keep their own per-step
//!    `tokio::time::timeout` + structured-error scaffolding around the raw
//!    `read_exact`/`write_all`.
//!  * [`write_len_prefixed`] / [`read_len_prefixed`] — async convenience
//!    wrappers for the plain factored sites (butler-deposit, tunnel-task).
//!
//! Endianness is a parameter so every shipped wire format is preserved
//! (ZEB-572 option a). **New framing should use [`Endian::Be`]** (network
//! order); the little-endian variant exists only for backward wire-compat with
//! already-deployed peers.

/// Byte order of the 4-byte `u32` length prefix. A parameter (not a constant)
/// because shipped protocols disagree and their on-the-wire bytes must be
/// preserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    /// Network order. The convention for all NEW framing.
    Be,
    /// Little-endian. Backward-compat only (butler-deposit, the invite/friend/
    /// pex acceptors, the open-join + invite-redeem dial paths).
    Le,
}

impl Endian {
    fn encode(self, len: u32) -> [u8; 4] {
        match self {
            Endian::Be => len.to_be_bytes(),
            Endian::Le => len.to_le_bytes(),
        }
    }

    fn decode(self, buf: [u8; 4]) -> u32 {
        match self {
            Endian::Be => u32::from_be_bytes(buf),
            Endian::Le => u32::from_le_bytes(buf),
        }
    }
}

/// A length prefix that is zero (when empty is disallowed) or exceeds the cap.
/// Raised BEFORE any body byte is read or allocated, so an attacker-supplied
/// prefix can never drive an allocation past `max`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("frame length {len} out of bounds (max {max})")]
pub struct FrameLenError {
    pub len: usize,
    pub max: usize,
}

/// Encode a body length into its 4-byte prefix. Rejects an empty body (unless
/// `allow_empty`) or one exceeding `max`, before producing any bytes.
pub fn encode_len_prefix(
    body_len: usize,
    max: usize,
    endian: Endian,
    allow_empty: bool,
) -> Result<[u8; 4], FrameLenError> {
    if (!allow_empty && body_len == 0) || body_len > max {
        return Err(FrameLenError { len: body_len, max });
    }
    // Lossless: body_len <= max, and every caller's cap is far below u32::MAX.
    Ok(endian.encode(body_len as u32))
}

/// Decode a 4-byte length prefix into the body length to read next. Rejects a
/// zero length (unless `allow_empty`) or one exceeding `max`, before the body
/// is read or allocated.
pub fn decode_len_prefix(
    buf: [u8; 4],
    max: usize,
    endian: Endian,
    allow_empty: bool,
) -> Result<usize, FrameLenError> {
    let len = endian.decode(buf) as usize;
    if (!allow_empty && len == 0) || len > max {
        return Err(FrameLenError { len, max });
    }
    Ok(len)
}

#[cfg(test)]
mod core_tests {
    use super::*;

    const MAX: usize = 256 * 1024;

    #[test]
    fn encode_rejects_empty_when_disallowed() {
        assert_eq!(
            encode_len_prefix(0, MAX, Endian::Le, false),
            Err(FrameLenError { len: 0, max: MAX })
        );
    }

    #[test]
    fn encode_accepts_empty_when_allowed() {
        assert_eq!(
            encode_len_prefix(0, MAX, Endian::Be, true),
            Ok([0, 0, 0, 0])
        );
    }

    #[test]
    fn encode_rejects_oversize() {
        assert_eq!(
            encode_len_prefix(MAX + 1, MAX, Endian::Le, false),
            Err(FrameLenError {
                len: MAX + 1,
                max: MAX
            })
        );
    }

    #[test]
    fn encode_accepts_at_cap() {
        assert!(encode_len_prefix(MAX, MAX, Endian::Le, false).is_ok());
    }

    #[test]
    fn decode_rejects_zero_when_disallowed() {
        assert_eq!(
            decode_len_prefix([0, 0, 0, 0], MAX, Endian::Le, false),
            Err(FrameLenError { len: 0, max: MAX })
        );
    }

    #[test]
    fn decode_accepts_zero_when_allowed() {
        assert_eq!(
            decode_len_prefix([0, 0, 0, 0], MAX, Endian::Be, true),
            Ok(0)
        );
    }

    #[test]
    fn decode_rejects_oversize_before_body() {
        let buf = ((MAX + 1) as u32).to_le_bytes();
        assert_eq!(
            decode_len_prefix(buf, MAX, Endian::Le, false),
            Err(FrameLenError {
                len: MAX + 1,
                max: MAX
            })
        );
    }

    #[test]
    fn decode_accepts_at_cap() {
        let buf = (MAX as u32).to_le_bytes();
        assert_eq!(decode_len_prefix(buf, MAX, Endian::Le, false), Ok(MAX));
    }

    #[test]
    fn be_and_le_differ_and_each_round_trips() {
        // Distinct bytes so the order is observable.
        let len = 0x0102_0304usize;
        let be = encode_len_prefix(len, usize::MAX, Endian::Be, false).unwrap();
        let le = encode_len_prefix(len, usize::MAX, Endian::Le, false).unwrap();
        assert_eq!(be, [0x01, 0x02, 0x03, 0x04]);
        assert_eq!(le, [0x04, 0x03, 0x02, 0x01]);
        assert_ne!(be, le);
        assert_eq!(
            decode_len_prefix(be, usize::MAX, Endian::Be, false).unwrap(),
            len
        );
        assert_eq!(
            decode_len_prefix(le, usize::MAX, Endian::Le, false).unwrap(),
            len
        );
    }
}
