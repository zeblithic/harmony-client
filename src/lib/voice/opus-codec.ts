/**
 * OpusCodec — thin wrapper around opusscript (emscripten-compiled libopus).
 *
 * Package: `opusscript` (npm)
 * API adaptation notes:
 *   - opusscript uses a single object for both encode and decode (not separate
 *     encoder/decoder instances).
 *   - encode(buffer, frameSize) → Buffer  (int16 PCM in, Opus bytes out)
 *   - decode(buffer) → Buffer             (Opus bytes in, int16 PCM out)
 *   - delete() frees the Emscripten heap allocation
 *   - Input/output use Node Buffer / Uint8Array; float PCM is converted to/from
 *     int16 by this wrapper so callers always work with Float32Array.
 *
 * Voice pipeline context:
 *   16 kHz mono, 20 ms frames → 320 samples per frame.
 */

// Dynamic import so the WASM binary is loaded lazily and test mocks intercept
// the import before the module executes.
type OpusScriptModule = {
  default: OpusScriptConstructor;
};

type OpusScriptConstructor = {
  new (
    samplingRate: number,
    channels: number,
    application: number,
    options?: { wasm?: boolean },
  ): OpusScriptInstance;
  Application: { VOIP: number; AUDIO: number; RESTRICTED_LOWDELAY: number };
};

interface OpusScriptInstance {
  encode(buffer: Buffer, frameSize: number): Buffer;
  decode(buffer: Buffer): Buffer;
  delete(): void;
}

export class OpusCodec {
  private _codec: OpusScriptInstance | null = null;
  private _sampleRate = 0;
  private _channels = 0;

  /**
   * Load the WASM module and create the encoder/decoder.
   * Must be called before encode() or decode().
   */
  async init(sampleRate: number, channels: number): Promise<void> {
    this._sampleRate = sampleRate;
    this._channels = channels;

    const mod = (await import('opusscript')) as OpusScriptModule;
    const OpusScript = mod.default;
    this._codec = new OpusScript(sampleRate, channels, OpusScript.Application.VOIP);
  }

  /**
   * Encode a Float32Array of PCM samples into an Opus packet.
   * Throws 'not initialized' if init() has not been called.
   *
   * The frame size is derived from the PCM length (number of samples per channel).
   */
  encode(pcm: Float32Array): Uint8Array {
    if (this._codec === null) {
      throw new Error('not initialized');
    }

    // opusscript expects int16 PCM in a Buffer
    const int16 = float32ToInt16(pcm);
    const buf = Buffer.from(int16.buffer, int16.byteOffset, int16.byteLength);
    const frameSize = pcm.length / this._channels;
    const encoded: Buffer = this._codec.encode(buf, frameSize);
    return new Uint8Array(encoded.buffer, encoded.byteOffset, encoded.byteLength);
  }

  /**
   * Decode an Opus packet into a Float32Array of PCM samples.
   * Throws 'not initialized' if init() has not been called.
   */
  decode(opusFrame: Uint8Array): Float32Array {
    if (this._codec === null) {
      throw new Error('not initialized');
    }

    const buf = Buffer.from(opusFrame.buffer, opusFrame.byteOffset, opusFrame.byteLength);
    const decoded: Buffer = this._codec.decode(buf);
    // decoded is raw int16 PCM bytes
    const int16 = new Int16Array(decoded.buffer, decoded.byteOffset, decoded.byteLength / 2);
    return int16ToFloat32(int16);
  }

  /**
   * Release Emscripten heap allocations. After destroy(), encode/decode throw.
   */
  destroy(): void {
    if (this._codec !== null) {
      this._codec.delete();
      this._codec = null;
    }
  }
}

// ---------------------------------------------------------------------------
// PCM format helpers
// ---------------------------------------------------------------------------

/** Convert Float32 [-1, 1] samples to Int16 [-32768, 32767]. */
function float32ToInt16(input: Float32Array): Int16Array {
  const output = new Int16Array(input.length);
  for (let i = 0; i < input.length; i++) {
    const s = Math.max(-1, Math.min(1, input[i]));
    output[i] = s < 0 ? s * 0x8000 : s * 0x7fff;
  }
  return output;
}

/** Convert Int16 samples to Float32 [-1, 1]. */
function int16ToFloat32(input: Int16Array): Float32Array {
  const output = new Float32Array(input.length);
  for (let i = 0; i < input.length; i++) {
    output[i] = input[i] / (input[i] < 0 ? 0x8000 : 0x7fff);
  }
  return output;
}
