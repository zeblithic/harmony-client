import { AdaptiveJitterBuffer } from './adaptive-jitter-buffer';
import { generateComfortNoise } from './comfort-noise';
import { decodeHeader, HEADER_SIZE } from './voice-packet';
import type { VoiceCodec, CodecType } from './voice-codec';

const FRAME_MS = 20;
const IDLE_TIMEOUT_MS = 2000;
/** Max frames queued during async codec init (~320ms of audio). */
const MAX_PENDING_FRAMES = 16;
const MIN_BUFFER_DEPTH = 2;
const MAX_BUFFER_DEPTH = 10;

interface PendingFrame {
  sequence: number;
  payload: Uint8Array;
  codec: CodecType;
}

/** Tracks a codec instance and its initialization state. */
interface CodecEntry {
  codec: VoiceCodec;
  ready: boolean;
  pendingFrames: PendingFrame[];
}

interface SenderState {
  jitterBuffer: AdaptiveJitterBuffer;
  /** One decoder per codec type, lazy-created on first frame of that type. */
  codecs: Map<CodecType, CodecEntry>;
  speaking: boolean;
  playbackTimer: ReturnType<typeof setInterval> | null;
  idleTimer: ReturnType<typeof setTimeout> | null;
  /** True once the initial codec has been initialized and playback timer started. */
  initialReady: boolean;
  /** Frame size for comfort noise (320 for opus, 160 for codec2). */
  frameSize: number;
}

export interface VoiceReceiverConfig {
  listen: (
    event: string,
    handler: (event: { payload: unknown }) => void,
  ) => Promise<() => void>;
  /** Factory that creates a codec instance for the given type. */
  createCodec: (codecType: CodecType) => VoiceCodec;
  onPlayFrame?: (senderHex: string, pcm: Float32Array | null) => void;
  /** Hex-encoded local sender hash — frames from this sender are filtered
   *  out to prevent self-echo (Zenoh delivers local puts to local subscribers). */
  ownSenderHex?: string;
}

export class VoiceReceiver {
  private config: VoiceReceiverConfig;
  private senders: Map<string, SenderState> = new Map();
  private unlisten: (() => void) | null = null;

  constructor(config: VoiceReceiverConfig) {
    this.config = config;
  }

  async init(): Promise<void> {
    this.unlisten = await this.config.listen(
      'voice-frame-received',
      (event) => this.handleFrame(event.payload as { frameBytes: number[] }),
    );
  }

  /** Map codec type to sample rate. */
  private static sampleRateFor(codecType: CodecType): number {
    return codecType === 'codec2' ? 8000 : 16000;
  }

  private handleFrame(payload: { frameBytes: number[] }): void {
    const bytes = new Uint8Array(payload.frameBytes);
    if (bytes.byteLength < HEADER_SIZE) return;

    const header = decodeHeader(bytes);
    const encodedPayload = bytes.slice(HEADER_SIZE);
    const senderHex = Array.from(header.senderHash)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');

    // Filter out own frames to prevent self-echo
    if (this.config.ownSenderHex && senderHex === this.config.ownSenderHex) return;

    let state = this.senders.get(senderHex);
    if (!state) {
      const entry = this.createCodecEntry(header.codec);
      const frameSize = header.codec === 'codec2' ? 160 : 320;
      state = {
        jitterBuffer: new AdaptiveJitterBuffer({
          minDepth: MIN_BUFFER_DEPTH,
          maxDepth: MAX_BUFFER_DEPTH,
          frameMs: FRAME_MS,
        }),
        codecs: new Map([[header.codec, entry]]),
        speaking: false,
        playbackTimer: null,
        idleTimer: null,
        initialReady: false,
        frameSize,
      };
      this.senders.set(senderHex, state);

      // Capture state reference for stale-closure guard
      const stateRef = state;
      entry.codec.init(VoiceReceiver.sampleRateFor(header.codec), 1).then(() => {
        if (this.senders.get(senderHex) !== stateRef) {
          stateRef.codecs.forEach((e) => e.codec.destroy());
          return;
        }
        entry.ready = true;
        stateRef.initialReady = true;
        // Drain pending frames for this codec
        this.drainPendingFrames(entry, stateRef.jitterBuffer);
        // Start playback timer only after initial codec is ready
        stateRef.playbackTimer = setInterval(() => {
          this.advancePlayback(senderHex);
        }, FRAME_MS);
      }).catch(() => {
        if (this.senders.get(senderHex) === stateRef) {
          this.removeSender(senderHex);
        }
      });
    }

    // Detect new PTT session: the sender stopped (PTT=false / tail frames)
    // and has now started speaking again. Reset the jitter buffer so it
    // re-seeds playSeq from the new sequence number.
    if (header.pttActive && !state.speaking && state.initialReady) {
      state.jitterBuffer.reset();
    }
    state.speaking = header.pttActive;

    if (state.idleTimer) clearTimeout(state.idleTimer);
    state.idleTimer = setTimeout(() => {
      this.removeSender(senderHex);
    }, IDLE_TIMEOUT_MS);

    // Get or create the codec entry for this frame's codec type
    const entry = this.getOrCreateCodecEntry(state, header.codec);

    if (entry.ready) {
      try {
        const pcm = entry.codec.decode(encodedPayload);
        state.jitterBuffer.insert(header.sequence, pcm);
      } catch {
        // Drop malformed frame
      }
    } else if (entry.pendingFrames.length < MAX_PENDING_FRAMES) {
      entry.pendingFrames.push({
        sequence: header.sequence,
        payload: encodedPayload,
        codec: header.codec,
      });
    }
  }

  /** Create a codec entry and start async init. */
  private createCodecEntry(codecType: CodecType): CodecEntry {
    return {
      codec: this.config.createCodec(codecType),
      ready: false,
      pendingFrames: [],
    };
  }

  /**
   * Get or lazily create a codec entry for the given codec type.
   * New codecs are initialized asynchronously — frames queue until ready.
   */
  private getOrCreateCodecEntry(state: SenderState, codecType: CodecType): CodecEntry {
    let entry = state.codecs.get(codecType);
    if (!entry) {
      entry = this.createCodecEntry(codecType);
      state.codecs.set(codecType, entry);
      const prevFrameSize = state.frameSize;
      state.frameSize = codecType === 'codec2' ? 160 : 320;

      // Start async init — drain pending frames when ready
      const entryRef = entry;
      entry.codec.init(VoiceReceiver.sampleRateFor(codecType), 1).then(() => {
        entryRef.ready = true;
        this.drainPendingFrames(entryRef, state.jitterBuffer);
      }).catch(() => {
        entryRef.codec.destroy();
        state.codecs.delete(codecType);
        // Restore frame size if this was the only codec of this type
        state.frameSize = prevFrameSize;
      });
    }
    return entry;
  }

  /** Decode and insert all pending frames for a codec entry. */
  private drainPendingFrames(entry: CodecEntry, jitterBuffer: AdaptiveJitterBuffer): void {
    for (const pf of entry.pendingFrames) {
      try {
        const pcm = entry.codec.decode(pf.payload);
        jitterBuffer.insert(pf.sequence, pcm);
      } catch {
        // Drop undecodable frame
      }
    }
    entry.pendingFrames = [];
  }

  private advancePlayback(senderHex: string): void {
    const state = this.senders.get(senderHex);
    if (!state) return;
    let pcm = state.jitterBuffer.advance();
    // Generate comfort noise for missing frames only while sender is speaking
    if (pcm === null && state.speaking && state.jitterBuffer.isReady()) {
      pcm = generateComfortNoise(state.frameSize, 0.005);
    }
    this.config.onPlayFrame?.(senderHex, pcm);
  }

  private removeSender(senderHex: string): void {
    const state = this.senders.get(senderHex);
    if (!state) return;
    if (state.playbackTimer) clearInterval(state.playbackTimer);
    if (state.idleTimer) clearTimeout(state.idleTimer);
    state.codecs.forEach((e) => e.codec.destroy());
    this.senders.delete(senderHex);
  }

  getActiveSenders(): string[] {
    return Array.from(this.senders.keys());
  }

  isSpeaking(senderHex: string): boolean {
    return this.senders.get(senderHex)?.speaking ?? false;
  }

  destroy(): void {
    if (this.unlisten) {
      this.unlisten();
      this.unlisten = null;
    }
    for (const key of [...this.senders.keys()]) {
      this.removeSender(key);
    }
  }
}
