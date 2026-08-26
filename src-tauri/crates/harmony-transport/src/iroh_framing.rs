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

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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
    // The wire prefix is a u32: reject (never silently truncate) a body that
    // can't be represented, so the audited boundary stays safe even when a
    // caller passes a `max` at/above u32::MAX (the no-cap write sites).
    let len = u32::try_from(body_len).map_err(|_| FrameLenError { len: body_len, max })?;
    Ok(endian.encode(len))
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

/// An error from the async framing wrappers: either the body length was out of
/// bounds (the cap guard fired, before any I/O on the body) or the underlying
/// stream returned an I/O error.
#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    #[error(transparent)]
    OutOfBounds(#[from] FrameLenError),
    #[error("frame I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Write a `[u32 prefix][body]` frame. The bound check ([`encode_len_prefix`])
/// runs first, so an out-of-bounds body is rejected before anything is written.
/// Prefix then body are written as two `write_all`s (each all-or-error), which
/// avoids copying the body into a temporary frame buffer — material for the
/// large-cap relay-pull writer (`RELAY_PULL_MAX_FRAME_BYTES`, 16 MiB).
pub async fn write_len_prefixed<W: AsyncWrite + Unpin>(
    w: &mut W,
    body: &[u8],
    max: usize,
    endian: Endian,
    allow_empty: bool,
) -> Result<(), FramingError> {
    let prefix = encode_len_prefix(body.len(), max, endian, allow_empty)?;
    w.write_all(&prefix).await?;
    w.write_all(body).await?;
    Ok(())
}

/// Read a `[u32 prefix][body]` frame. The prefix is decoded and bound-checked
/// ([`decode_len_prefix`]) before the body is allocated or read, so an
/// attacker-supplied prefix never drives an allocation past `max`.
pub async fn read_len_prefixed<R: AsyncRead + Unpin>(
    r: &mut R,
    max: usize,
    endian: Endian,
    allow_empty: bool,
) -> Result<Vec<u8>, FramingError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = decode_len_prefix(len_buf, max, endian, allow_empty)?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(body)
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
    #[cfg(target_pointer_width = "64")]
    fn encode_rejects_body_exceeding_u32_even_with_huge_max() {
        // A body too large for the 4-byte wire prefix is rejected, not
        // truncated, even when `max` is at/above u32::MAX (the no-cap sites).
        let too_big = u32::MAX as usize + 1;
        assert_eq!(
            encode_len_prefix(too_big, usize::MAX, Endian::Le, false),
            Err(FrameLenError {
                len: too_big,
                max: usize::MAX
            })
        );
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

#[cfg(test)]
mod wrapper_tests {
    use super::*;

    const MAX: usize = 1024;

    #[tokio::test]
    async fn round_trip_le_nonempty() {
        let body = b"hello frame".to_vec();
        let mut buf = Vec::new();
        write_len_prefixed(&mut buf, &body, MAX, Endian::Le, false)
            .await
            .unwrap();
        assert_eq!(&buf[..4], &(body.len() as u32).to_le_bytes());
        let mut reader = buf.as_slice();
        let got = read_len_prefixed(&mut reader, MAX, Endian::Le, false)
            .await
            .unwrap();
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn round_trip_be_allow_empty_zero_length() {
        let mut buf = Vec::new();
        write_len_prefixed(&mut buf, &[], MAX, Endian::Be, true)
            .await
            .unwrap();
        assert_eq!(buf, vec![0, 0, 0, 0]);
        let mut reader = buf.as_slice();
        let got = read_len_prefixed(&mut reader, MAX, Endian::Be, true)
            .await
            .unwrap();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn read_rejects_oversize_before_body() {
        // Prefix only, no body: an honest reader would block/EOF on the body.
        // The cap guard must reject from the prefix alone (OutOfBounds, not Io).
        let prefix_only = ((MAX + 1) as u32).to_le_bytes().to_vec();
        let mut reader = prefix_only.as_slice();
        let err = read_len_prefixed(&mut reader, MAX, Endian::Le, false)
            .await
            .expect_err("oversize prefix must be rejected");
        assert!(
            matches!(err, FramingError::OutOfBounds(FrameLenError { len, max }) if len == MAX + 1 && max == MAX),
            "expected OutOfBounds rejected before body read, got {err:?}"
        );
    }

    #[tokio::test]
    async fn write_rejects_oversize_writes_nothing() {
        let body = vec![0u8; MAX + 1];
        let mut buf = Vec::new();
        let err = write_len_prefixed(&mut buf, &body, MAX, Endian::Le, false)
            .await
            .expect_err("oversize body must be rejected");
        assert!(matches!(err, FramingError::OutOfBounds(_)));
        assert!(buf.is_empty(), "nothing should be written on rejection");
    }

    #[tokio::test]
    async fn write_rejects_empty_when_disallowed() {
        let mut buf = Vec::new();
        let err = write_len_prefixed(&mut buf, &[], MAX, Endian::Le, false)
            .await
            .expect_err("empty body must be rejected when not allowed");
        assert!(matches!(
            err,
            FramingError::OutOfBounds(FrameLenError { len: 0, .. })
        ));
        assert!(buf.is_empty());
    }
}
