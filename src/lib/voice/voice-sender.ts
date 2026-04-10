import { type AudioCapture } from './audio-capture';
import { type OpusCodec } from './opus-codec';
import { encodeHeader, HEADER_SIZE } from './voice-packet';

const FRAME_MS = 20;
const TAIL_FRAME_COUNT = 3;

export interface VoiceSenderConfig {
  /** 16-byte node address (Reticulum-compatible sender hash). */
  senderHash: Uint8Array;
  /** Voice channel ID used as the Zenoh topic segment. */
  channelId: string;
  /** Tauri invoke function — wraps window.__TAURI__.invoke in production. */
  invoke: (cmd: string, args: Record<string, unknown>) => Promise<unknown>;
  /** Opus encoder/decoder instance. */
  codec: OpusCodec;
  /** AudioWorklet-based PCM capture service. */
  capture: AudioCapture;
}

/**
 * VoiceSender — outbound voice orchestrator.
 *
 * Connects AudioCapture (microphone frames) → OpusCodec (compression) →
 * voice-packet header assembly → Tauri IPC → Rust publish on Zenoh.
 *
 * Usage:
 *   await sender.start();   // begin capturing + streaming
 *   await sender.stop();    // flush tail frames, release resources
 */
export class VoiceSender {
  private config: VoiceSenderConfig;
  private sequence = 0;
  private timestamp = 0;
  private active = false;
  /** Non-null while async initialization is in progress. */
  private starting: Promise<void> | null = null;

  constructor(config: VoiceSenderConfig) {
    this.config = config;
  }

  /**
   * Initialize the codec, start audio capture, and begin streaming frames.
   * Idempotent — calling start() while already active or starting is a no-op.
   *
   * Sequence and timestamp persist across PTT sessions so the receiver's
   * jitter buffer sees a continuous stream rather than a reset to zero.
   */
  async start(): Promise<void> {
    if (this.active) return;
    if (this.starting) return this.starting;
    this.starting = (async () => {
      await this.config.codec.init(16000, 1);
      try {
        await this.config.capture.start((pcm) => this.sendFrame(pcm, true));
      } catch (err) {
        // Codec initialized but capture failed — clean up codec to prevent leak
        this.config.codec.destroy();
        throw err;
      }
      this.active = true;
    })();
    try {
      await this.starting;
    } finally {
      this.starting = null;
    }
  }

  /**
   * Stop audio capture and flush three tail frames with PTT=false so that
   * receivers can cleanly detect end-of-transmission. Releases codec memory.
   * Idempotent — calling stop() while not active is a no-op.
   */
  async stop(): Promise<void> {
    // Wait for any in-progress start to finish before tearing down.
    if (this.starting) await this.starting.catch(() => {});
    if (!this.active) return;
    await this.config.capture.stop();
    // Send TAIL_FRAME_COUNT silence frames with PTT=false so receivers
    // know the push-to-talk session has ended. Awaited so tail frames
    // reach Rust/Zenoh before codec is destroyed.
    const silence = new Float32Array(320);
    for (let i = 0; i < TAIL_FRAME_COUNT; i++) {
      await this.sendFrame(silence, false);
    }
    this.config.codec.destroy();
    this.active = false;
  }

  /**
   * Encode one PCM frame, prepend the voice-packet header, and send to
   * the Rust backend via Tauri IPC.
   *
   * Returns the IPC promise. In the audio callback hot path, the return
   * value is ignored (fire-and-forget). In stop(), tail frames are awaited
   * to ensure end-of-transmission reaches the mesh before teardown.
   *
   * The sequence number wraps at 65535 (u16); the timestamp accumulates
   * FRAME_MS (20 ms) per frame and wraps naturally as a u32.
   */
  private sendFrame(pcm: Float32Array, pttActive: boolean): Promise<unknown> {
    const opus = this.config.codec.encode(pcm);
    const header = encodeHeader({
      pttActive,
      sequence: this.sequence & 0xffff,
      timestamp: this.timestamp >>> 0,
      senderHash: this.config.senderHash,
    });
    const frame = new Uint8Array(HEADER_SIZE + opus.byteLength);
    frame.set(header, 0);
    frame.set(opus, HEADER_SIZE);
    // Tauri v2 deserializes invoke args by parameter name — the Rust command
    // parameter is `payload: SendVoiceFramePayload`, so we wrap accordingly.
    // Uint8Array → number[] for JSON serialization over IPC.
    const promise = this.config.invoke('send_voice_frame', {
      payload: {
        channelId: this.config.channelId,
        frameBytes: Array.from(frame),
      },
    }).catch(() => {
      // IPC errors are non-fatal for individual frames
    });
    this.sequence = (this.sequence + 1) & 0xffff;
    this.timestamp += FRAME_MS;
    return promise;
  }
}
