import { type AudioCapture } from './audio-capture';
import { type VoiceCodec } from './voice-codec';
import { encodeHeader, HEADER_SIZE } from './voice-packet';

const FRAME_MS = 20;
const TAIL_FRAME_COUNT = 3;

export interface VoiceSenderConfig {
  /** 16-byte node address (Reticulum-compatible sender hash). */
  senderHash: Uint8Array;
  /** Community ID that scopes the voice channel (publish scope). */
  communityId: string;
  /** Voice channel ID used as the Zenoh topic segment. */
  channelId: string;
  /** Tauri invoke function — wraps window.__TAURI__.invoke in production. */
  invoke: (cmd: string, args: Record<string, unknown>) => Promise<unknown>;
  /** Voice codec instance (OpusCodec or Codec2Codec). */
  codec: VoiceCodec;
  /** AudioWorklet-based PCM capture service. */
  capture: AudioCapture;
  /** Sample rate for audio capture and codec init. Default: 16000. */
  sampleRate?: number;
  /**
   * Optional per-frame gate. Called with each captured PCM frame; returns
   * whether to transmit it and the PTT bit to stamp. Absent ⇒ always
   * { send: true, ptt: true } (legacy behavior). The session controller
   * supplies a gate encoding mute / PTT / VAD (DTX).
   */
  frameGate?: (pcm: Float32Array) => { send: boolean; ptt: boolean };
  /**
   * Optional transport override for publishing an assembled voice frame. When
   * present it replaces the default `invoke('send_voice_frame', …)` call —
   * the DM call controller supplies one that addresses by `callId` via
   * `send_dm_voice_frame`. Receives the raw frame bytes (header + payload) as a
   * plain number[] (JSON-serializable over IPC). Absent ⇒ legacy channel path.
   */
  publishFrame?: (frameBytes: number[]) => Promise<unknown>;
  /**
   * ZEB-359: preferred capture device provider, re-read at every capture
   * (re)start so a settings change picks up without reconstructing the sender.
   * Absent / returning null ⇒ system default.
   */
  inputDeviceId?: () => string | null;
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
  /** The frame callback handed to capture, retained so a device switch can
   *  restart capture without touching codec state (ZEB-359). */
  private onFrameCb: ((pcm: Float32Array) => void) | null = null;

  constructor(config: VoiceSenderConfig) {
    this.config = config;
  }

  private currentInputDevice(): string | null {
    return this.config.inputDeviceId ? this.config.inputDeviceId() : null;
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
      const sr = this.config.sampleRate ??
        (this.config.codec.codecType === 'codec2' ? 8000 : 16000);
      await this.config.codec.init(sr, 1);
      try {
        this.onFrameCb = (pcm) => {
          const decision = this.config.frameGate
            ? this.config.frameGate(pcm)
            : { send: true, ptt: true };
          if (decision.send) this.sendFrame(pcm, decision.ptt);
          // Advance the stream clock for EVERY captured frame, even gated
          // (DTX / VAD-silence) ones we don't transmit. The receiver's jitter
          // buffer advances its playhead every 20 ms regardless of arrivals;
          // if the sender froze its sequence across a silence gap, the first
          // resumed frame would land "behind" the playhead and be discarded
          // as late — silently killing speech after every pause. Advancing
          // per captured frame keeps sequence/timestamp aligned with
          // wall-clock playback so resumed audio stays in sync.
          this.advanceClock();
        };
        await this.config.capture.start(
          this.onFrameCb,
          undefined,
          undefined,
          sr,
          this.currentInputDevice(),
        );
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
   * ZEB-359: live input-device switch — restart ONLY the capture leg with the
   * current preferred device. Codec state and the stream clock survive, so the
   * receiver just sees a short silence gap (same shape as a DTX pause).
   *
   * On capture-restart failure the sender deactivates (codec released, error
   * surfaced) rather than pretending audio still flows; callers reuse their
   * existing mic-error handling and a later start() re-initializes fully.
   */
  async switchInputDevice(): Promise<void> {
    if (this.starting) await this.starting.catch(() => {});
    if (!this.active || !this.onFrameCb) return;
    // Register in the `starting` mutex (PR #495 R1): a concurrent stop()
    // (session teardown) awaits `starting` first, so it can't destroy the
    // codec under this restart or leave the freshly restarted capture running
    // (open mic) after teardown — it tears the new capture down itself once
    // the switch settles.
    this.starting = (async () => {
      const cb = this.onFrameCb!;
      const sr = this.config.sampleRate ??
        (this.config.codec.codecType === 'codec2' ? 8000 : 16000);
      await this.config.capture.stop();
      try {
        await this.config.capture.start(
          cb,
          undefined,
          undefined,
          sr,
          this.currentInputDevice(),
        );
      } catch (err) {
        this.config.codec.destroy();
        this.active = false;
        this.onFrameCb = null;
        throw err;
      }
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
    const sr = this.config.sampleRate ??
      (this.config.codec.codecType === 'codec2' ? 8000 : 16000);
    const frameSize = Math.round(sr * 0.02);
    const silence = new Float32Array(frameSize);
    for (let i = 0; i < TAIL_FRAME_COUNT; i++) {
      await this.sendFrame(silence, false);
      this.advanceClock();
    }
    this.config.codec.destroy();
    this.active = false;
    this.onFrameCb = null;
  }

  /**
   * Encode one PCM frame, prepend the voice-packet header, and send to
   * the Rust backend via Tauri IPC.
   *
   * Returns the IPC promise. In the audio callback hot path, the return
   * value is ignored (fire-and-forget). In stop(), tail frames are awaited
   * to ensure end-of-transmission reaches the mesh before teardown.
   *
   * Stamps the CURRENT stream clock (sequence/timestamp); advancing the clock
   * is the caller's job (advanceClock) so gated DTX frames still tick it.
   */
  private sendFrame(pcm: Float32Array, pttActive: boolean): Promise<unknown> {
    const encoded = this.config.codec.encode(pcm);
    const header = encodeHeader({
      pttActive,
      sequence: this.sequence & 0xffff,
      timestamp: this.timestamp >>> 0,
      senderHash: this.config.senderHash,
      codec: this.config.codec.codecType,
    });
    const frame = new Uint8Array(HEADER_SIZE + encoded.byteLength);
    frame.set(header, 0);
    frame.set(encoded, HEADER_SIZE);
    // Uint8Array → number[] for JSON serialization over IPC.
    const frameBytes = Array.from(frame);
    // A supplied publishFrame override replaces the default channel transport
    // (the DM call controller routes through send_dm_voice_frame by callId).
    // Tauri v2 deserializes invoke args by parameter name — the Rust command
    // parameter is `payload: SendVoiceFramePayload`, so we wrap accordingly.
    const publish = this.config.publishFrame
      ? this.config.publishFrame(frameBytes)
      : this.config.invoke('send_voice_frame', {
          payload: {
            communityId: this.config.communityId,
            channelId: this.config.channelId,
            frameBytes,
          },
        });
    return publish.catch(() => {
      // IPC errors are non-fatal for individual frames
    });
  }

  /**
   * Advance the RTP-style stream clock by one frame. Called once per CAPTURED
   * frame (transmitted or gated) so the sequence/timestamp track wall-clock
   * elapsed audio, not just sent packets. The sequence wraps at 65535 (u16);
   * the timestamp accumulates FRAME_MS (20 ms) per frame and wraps as a u32.
   */
  private advanceClock(): void {
    this.sequence = (this.sequence + 1) & 0xffff;
    this.timestamp = (this.timestamp + FRAME_MS) >>> 0;
  }
}
