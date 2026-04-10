/** Size of the voice packet header in bytes. */
export const HEADER_SIZE = 23;

/** Protocol version encoded in the upper nibble of the flags byte. */
export const VOICE_VERSION = 0x01;

/** Fields required to encode a voice packet header. */
export interface VoiceHeaderFields {
  /** Whether the push-to-talk button is currently active. */
  pttActive: boolean;
  /** Monotonically increasing frame sequence number (u16, wraps at 65535). */
  sequence: number;
  /** Milliseconds elapsed since the start of this voice stream (u32). */
  timestamp: number;
  /**
   * 16-byte sender address hash derived from the sender's Ed25519/X25519
   * public keys (SHA-256 truncated to 128 bits, Reticulum-compatible).
   * Only the first 16 bytes are used; any excess is silently ignored.
   */
  senderHash: Uint8Array;
}

/** Decoded representation of a voice packet header. */
export interface DecodedVoiceHeader {
  /** Protocol version from the upper nibble of byte 0. */
  version: number;
  /** Whether push-to-talk was active when the packet was sent. */
  pttActive: boolean;
  /** Frame sequence number (u16). */
  sequence: number;
  /** Milliseconds since stream start (u32). */
  timestamp: number;
  /** 16-byte sender address hash (copy of bytes 7–22). */
  senderHash: Uint8Array;
}

/**
 * Encode a 23-byte voice packet header.
 *
 * Header layout:
 *   Byte 0:      [4-bit version=0x1][PTT flag][3 reserved bits]
 *   Bytes 1–2:   sequence (u16 big-endian)
 *   Bytes 3–6:   timestamp (u32 big-endian, ms since stream start)
 *   Bytes 7–22:  sender address hash (16 bytes)
 */
export function encodeHeader(fields: VoiceHeaderFields): Uint8Array {
  const buf = new Uint8Array(HEADER_SIZE);
  const view = new DataView(buf.buffer);

  // Byte 0: version nibble | PTT bit | 3 reserved bits
  const flags = (VOICE_VERSION << 4) | (fields.pttActive ? 0x08 : 0x00);
  buf[0] = flags;

  // Bytes 1–2: sequence (big-endian u16)
  view.setUint16(1, fields.sequence, false);

  // Bytes 3–6: timestamp (big-endian u32)
  view.setUint32(3, fields.timestamp, false);

  // Bytes 7–22: sender hash (take first 16 bytes)
  buf.set(fields.senderHash.subarray(0, 16), 7);

  return buf;
}

/**
 * Decode a 23-byte voice packet header from `buf`.
 *
 * @throws {RangeError} if `buf.length < HEADER_SIZE`.
 */
export function decodeHeader(buf: Uint8Array): DecodedVoiceHeader {
  if (buf.length < HEADER_SIZE) {
    throw new RangeError(
      `Voice packet buffer too short: expected at least ${HEADER_SIZE} bytes, got ${buf.length}`
    );
  }

  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);

  const flagsByte = buf[0];
  const version = (flagsByte >> 4) & 0x0f;
  const pttActive = (flagsByte & 0x08) !== 0;
  const sequence = view.getUint16(1, false);
  const timestamp = view.getUint32(3, false);
  const senderHash = buf.slice(7, 23);

  return { version, pttActive, sequence, timestamp, senderHash };
}
