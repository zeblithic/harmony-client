import { describe, it, expect } from 'vitest';
import {
  HEADER_SIZE,
  VOICE_VERSION,
  encodeHeader,
  decodeHeader,
  type VoiceHeaderFields,
} from './voice-packet';

/** Reusable 16-byte sender address hash used across tests. */
function makeSenderHash(): Uint8Array {
  const senderHash = new Uint8Array(16);
  for (let i = 0; i < 16; i++) senderHash[i] = 0xaa + i;
  return senderHash;
}

describe('voice-packet header', () => {
  it('HEADER_SIZE is 23', () => {
    expect(HEADER_SIZE).toBe(23);
  });

  it('encodes and decodes a header roundtrip', () => {
    const senderHash = makeSenderHash();
    const fields: VoiceHeaderFields = {
      pttActive: true,
      sequence: 42,
      timestamp: 1000,
      senderHash,
    };

    const buf = encodeHeader(fields);
    const decoded = decodeHeader(buf);

    expect(decoded.version).toBe(VOICE_VERSION);
    expect(decoded.pttActive).toBe(true);
    expect(decoded.sequence).toBe(42);
    expect(decoded.timestamp).toBe(1000);
    expect(decoded.senderHash).toEqual(senderHash);
  });

  it('encodes PTT inactive flag', () => {
    const senderHash = makeSenderHash();
    const fields: VoiceHeaderFields = {
      pttActive: false,
      sequence: 0,
      timestamp: 0,
      senderHash,
    };

    const buf = encodeHeader(fields);
    const decoded = decodeHeader(buf);

    expect(decoded.pttActive).toBe(false);
    // PTT bit (0x08) must be clear; version nibble must still be correct
    expect(decoded.version).toBe(VOICE_VERSION);
    // Verify raw flags byte directly
    expect(buf[0] & 0x08).toBe(0);
  });

  it('encodes max sequence number (65535)', () => {
    const fields: VoiceHeaderFields = {
      pttActive: false,
      sequence: 65535,
      timestamp: 0,
      senderHash: makeSenderHash(),
    };

    const buf = encodeHeader(fields);
    const decoded = decodeHeader(buf);

    expect(decoded.sequence).toBe(65535);
  });

  it('encodes large timestamp (0xFFFFFFFF)', () => {
    const fields: VoiceHeaderFields = {
      pttActive: false,
      sequence: 0,
      timestamp: 0xffffffff,
      senderHash: makeSenderHash(),
    };

    const buf = encodeHeader(fields);
    const decoded = decodeHeader(buf);

    expect(decoded.timestamp).toBe(0xffffffff);
  });

  it('builds a full packet with opus payload', () => {
    const senderHash = makeSenderHash();
    const fields: VoiceHeaderFields = {
      pttActive: true,
      sequence: 7,
      timestamp: 500,
      senderHash,
    };

    const header = encodeHeader(fields);
    const opusPayload = new Uint8Array([0xde, 0xad, 0xbe, 0xef]);

    // Assemble full packet
    const packet = new Uint8Array(HEADER_SIZE + opusPayload.length);
    packet.set(header, 0);
    packet.set(opusPayload, HEADER_SIZE);

    // Header portion decodes correctly
    const decoded = decodeHeader(packet.subarray(0, HEADER_SIZE));
    expect(decoded.sequence).toBe(7);
    expect(decoded.timestamp).toBe(500);
    expect(decoded.pttActive).toBe(true);

    // Payload sits at correct offset
    expect(packet.subarray(HEADER_SIZE)).toEqual(opusPayload);
  });

  it('rejects buffer shorter than HEADER_SIZE', () => {
    const short = new Uint8Array(10);
    expect(() => decodeHeader(short)).toThrow();
  });
});
