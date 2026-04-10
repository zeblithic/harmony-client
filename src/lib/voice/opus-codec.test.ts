/**
 * Tests for OpusCodec — Opus WASM wrapper.
 *
 * The underlying opusscript module is mocked because WASM binaries do not
 * load reliably in jsdom. The mock provides the same constructor-based API
 * that opusscript exposes so the adapter logic is still exercised.
 */
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { OpusCodec } from './opus-codec';

// ---------------------------------------------------------------------------
// Mock opusscript
// ---------------------------------------------------------------------------

const mockEncode = vi.fn();
const mockDecode = vi.fn();
const mockDelete = vi.fn();

// The mock constructor must be a real function (not an arrow) so that `new`
// works and `this` binds correctly. Vitest requires the constructor to be
// declared before vi.mock() hoists it, so we use a class here.
//
// opusscript API:
//   const codec = new OpusScript(sampleRate, channels, OpusScript.Application.VOIP);
//   codec.encode(buffer, frameSize)  → Buffer
//   codec.decode(buffer)             → Buffer
//   codec.delete()

vi.mock('opusscript', () => {
  const MockOpusScript = vi.fn(function (this: Record<string, unknown>) {
    this.encode = mockEncode;
    this.decode = mockDecode;
    this.delete = mockDelete;
  });
  (MockOpusScript as unknown as Record<string, unknown>).Application = { VOIP: 2048 };
  return { default: MockOpusScript };
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('OpusCodec', () => {
  let codec: OpusCodec;

  beforeEach(() => {
    vi.clearAllMocks();
    codec = new OpusCodec();
  });

  it('encodes PCM to Opus bytes', async () => {
    // encode returns a Buffer with some bytes
    const fakeEncodedBuf = Buffer.from([0x01, 0x02, 0x03, 0x04, 0x05]);
    mockEncode.mockReturnValue(fakeEncodedBuf);

    await codec.init(16000, 1);
    const pcm = new Float32Array(320); // 20ms at 16kHz
    const result = codec.encode(pcm);

    expect(result).toBeInstanceOf(Uint8Array);
    expect(result.length).toBeGreaterThan(0);
  });

  it('decodes Opus bytes to PCM', async () => {
    // decode returns a Buffer representing 320 int16 samples (640 bytes)
    const int16Samples = new Int16Array(320);
    const fakeDecodedBuf = Buffer.from(int16Samples.buffer);
    mockDecode.mockReturnValue(fakeDecodedBuf);

    await codec.init(16000, 1);
    const opusFrame = new Uint8Array([0xf8, 0x00, 0x00]);
    const result = codec.decode(opusFrame);

    expect(result).toBeInstanceOf(Float32Array);
    expect(result.length).toBe(320);
  });

  it('throws if encode called before init', () => {
    const pcm = new Float32Array(320);
    expect(() => codec.encode(pcm)).toThrow('not initialized');
  });

  it('throws if decode called before init', () => {
    const frame = new Uint8Array([0xf8, 0x00]);
    expect(() => codec.decode(frame)).toThrow('not initialized');
  });

  it('destroy cleans up resources', async () => {
    mockEncode.mockReturnValue(Buffer.from([0x01]));

    await codec.init(16000, 1);
    codec.destroy();

    const pcm = new Float32Array(320);
    expect(() => codec.encode(pcm)).toThrow('not initialized');
  });
});
