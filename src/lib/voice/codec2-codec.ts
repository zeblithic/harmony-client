/**
 * Codec2Codec — wrapper around emscripten-compiled codec2 WASM.
 *
 * Uses codec2 3200 mode: 8kHz, 20ms frames, 160 samples, 8 bytes output.
 *
 * Follows the same pattern as OpusCodec:
 * - Lazy WASM load via dynamic import
 * - Emscripten heap buffers for zero-copy encode/decode
 * - Explicit destroy() for heap cleanup
 */
import type { VoiceCodec, CodecType } from './voice-codec';

/** codec2 mode constant for 3200 bps. */
const CODEC2_MODE_3200 = 0;

interface Codec2Module {
  _codec2_create(mode: number): number;
  _codec2_encode(state: number, bits: number, speech: number): void;
  _codec2_decode(state: number, speech: number, bits: number): void;
  _codec2_destroy(state: number): void;
  _codec2_samples_per_frame(state: number): number;
  _codec2_bits_per_frame(state: number): number;
  _malloc(size: number): number;
  _free(ptr: number): void;
  HEAP16: Int16Array;
  HEAPU8: Uint8Array;
}

export class Codec2Codec implements VoiceCodec {
  readonly codecType: CodecType = 'codec2';

  private module: Codec2Module | null = null;
  private statePtr = 0;
  private speechPtr = 0;
  private bitsPtr = 0;
  private samplesPerFrame = 0;
  private bytesPerFrame = 0;

  async init(_sampleRate: number, _channels: number): Promise<void> {
    if (this.module !== null) {
      this.destroy();
    }

    const loader = (await import('./codec2-wasm')).default;
    this.module = await loader() as Codec2Module;

    this.statePtr = this.module._codec2_create(CODEC2_MODE_3200);
    this.samplesPerFrame = this.module._codec2_samples_per_frame(this.statePtr);
    const bitsPerFrame = this.module._codec2_bits_per_frame(this.statePtr);
    this.bytesPerFrame = Math.ceil(bitsPerFrame / 8);

    // Allocate heap buffers: speech (int16) and bits (uint8)
    this.speechPtr = this.module._malloc(this.samplesPerFrame * 2); // int16 = 2 bytes
    if (this.speechPtr === 0) {
      this.module._codec2_destroy(this.statePtr);
      this.module = null;
      this.statePtr = 0;
      throw new Error('Failed to allocate codec2 speech buffer');
    }
    this.bitsPtr = this.module._malloc(this.bytesPerFrame);
    if (this.bitsPtr === 0) {
      this.module._free(this.speechPtr);
      this.module._codec2_destroy(this.statePtr);
      this.module = null;
      this.statePtr = 0;
      this.speechPtr = 0;
      throw new Error('Failed to allocate codec2 bits buffer');
    }
  }

  encode(pcm: Float32Array): Uint8Array {
    if (this.module === null) throw new Error('not initialized');

    // Convert float32 [-1,1] to int16 and write to heap
    const offset = this.speechPtr >> 1; // int16 index
    for (let i = 0; i < this.samplesPerFrame; i++) {
      const s = Math.max(-1, Math.min(1, pcm[i] ?? 0));
      this.module.HEAP16[offset + i] = s < 0 ? s * 0x8000 : s * 0x7fff;
    }

    this.module._codec2_encode(this.statePtr, this.bitsPtr, this.speechPtr);

    // Copy encoded bytes out of heap
    return new Uint8Array(
      this.module.HEAPU8.buffer,
      this.bitsPtr,
      this.bytesPerFrame,
    ).slice();
  }

  decode(encoded: Uint8Array): Float32Array {
    if (this.module === null) throw new Error('not initialized');

    // Write encoded bytes to heap
    this.module.HEAPU8.set(encoded.subarray(0, this.bytesPerFrame), this.bitsPtr);

    this.module._codec2_decode(this.statePtr, this.speechPtr, this.bitsPtr);

    // Read int16 from heap and convert to float32
    const offset = this.speechPtr >> 1;
    const out = new Float32Array(this.samplesPerFrame);
    for (let i = 0; i < this.samplesPerFrame; i++) {
      const sample = this.module.HEAP16[offset + i];
      out[i] = sample / (sample < 0 ? 0x8000 : 0x7fff);
    }
    return out;
  }

  destroy(): void {
    if (this.module === null) return;
    this.module._codec2_destroy(this.statePtr);
    this.module._free(this.speechPtr);
    this.module._free(this.bitsPtr);
    this.module = null;
    this.statePtr = 0;
    this.speechPtr = 0;
    this.bitsPtr = 0;
  }
}
