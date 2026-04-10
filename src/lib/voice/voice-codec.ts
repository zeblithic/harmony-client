/** Codec type identifier carried in voice packet headers. */
export type CodecType = 'opus' | 'codec2';

/**
 * Common interface for voice codecs used by VoiceSender and VoiceReceiver.
 *
 * Both OpusCodec and Codec2Codec implement this interface so the voice
 * pipeline can work with either codec interchangeably.
 */
export interface VoiceCodec {
  /** Load WASM and initialize encoder/decoder state. */
  init(sampleRate: number, channels: number): Promise<void>;
  /** Encode PCM float samples to compressed bytes. */
  encode(pcm: Float32Array): Uint8Array;
  /** Decode compressed bytes to PCM float samples. */
  decode(encoded: Uint8Array): Float32Array;
  /** Release WASM heap allocations. encode/decode throw after this. */
  destroy(): void;
  /** Identifies the codec for packet header encoding. */
  readonly codecType: CodecType;
}
