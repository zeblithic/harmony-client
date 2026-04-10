/**
 * Tests for Codec2Codec — codec2 WASM wrapper.
 *
 * The underlying codec2 emscripten module is mocked (same pattern as
 * opus-codec.test.ts) because WASM binaries don't load in jsdom.
 */
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { Codec2Codec } from './codec2-codec';

// ---------------------------------------------------------------------------
// Mock codec2 WASM module
// ---------------------------------------------------------------------------

const mockCreate = vi.fn().mockReturnValue(1); // state pointer
const mockEncode = vi.fn();
const mockDecode = vi.fn();
const mockDestroy = vi.fn();
const mockSamplesPerFrame = vi.fn().mockReturnValue(160);
const mockBitsPerFrame = vi.fn().mockReturnValue(64);
const mockMalloc = vi.fn().mockReturnValue(1024); // heap pointer
const mockFree = vi.fn();

const mockHEAP16 = new Int16Array(4096);
const mockHEAPU8 = new Uint8Array(mockHEAP16.buffer);

vi.mock('./codec2-wasm', () => {
  return {
    default: vi.fn().mockResolvedValue({
      _codec2_create: mockCreate,
      _codec2_encode: mockEncode,
      _codec2_decode: mockDecode,
      _codec2_destroy: mockDestroy,
      _codec2_samples_per_frame: mockSamplesPerFrame,
      _codec2_bits_per_frame: mockBitsPerFrame,
      _malloc: mockMalloc,
      _free: mockFree,
      HEAP16: mockHEAP16,
      HEAPU8: mockHEAPU8,
    }),
  };
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('Codec2Codec', () => {
  let codec: Codec2Codec;

  beforeEach(() => {
    vi.clearAllMocks();
    codec = new Codec2Codec();
  });

  it('exposes codecType as codec2', () => {
    expect(codec.codecType).toBe('codec2');
  });

  it('initializes WASM module and creates codec2 state', async () => {
    await codec.init(8000, 1);
    expect(mockCreate).toHaveBeenCalled();
    expect(mockMalloc).toHaveBeenCalled();
  });

  it('throws if encode called before init', () => {
    expect(() => codec.encode(new Float32Array(160))).toThrow('not initialized');
  });

  it('throws if decode called before init', () => {
    expect(() => codec.decode(new Uint8Array(8))).toThrow('not initialized');
  });

  it('encode accepts Float32Array and returns Uint8Array', async () => {
    await codec.init(8000, 1);
    const pcm = new Float32Array(160).fill(0.5);
    const result = codec.encode(pcm);
    expect(result).toBeInstanceOf(Uint8Array);
    expect(mockEncode).toHaveBeenCalled();
  });

  it('decode accepts Uint8Array and returns Float32Array', async () => {
    await codec.init(8000, 1);
    const encoded = new Uint8Array(8);
    const result = codec.decode(encoded);
    expect(result).toBeInstanceOf(Float32Array);
    expect(mockDecode).toHaveBeenCalled();
  });

  it('destroy frees resources and prevents further use', async () => {
    await codec.init(8000, 1);
    codec.destroy();
    expect(mockDestroy).toHaveBeenCalled();
    expect(mockFree).toHaveBeenCalled();
    expect(() => codec.encode(new Float32Array(160))).toThrow('not initialized');
  });

  it('destroy is safe to call multiple times', async () => {
    await codec.init(8000, 1);
    codec.destroy();
    codec.destroy(); // should not throw
    expect(mockDestroy).toHaveBeenCalledTimes(1);
  });
});
