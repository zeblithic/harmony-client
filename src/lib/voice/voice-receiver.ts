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

interface SenderState {
  jitterBuffer: AdaptiveJitterBuffer;
  /** One decoder per codec type, lazy-created on first frame of that type. */
  codecs: Map<CodecType, VoiceCodec>;
  speaking: boolean;
  playbackTimer: ReturnType<typeof setInterval> | null;
  idleTimer: ReturnType<typeof setTimeout> | null;
  /** False until codec.init() resolves. Frames are queued while false. */
  ready: boolean;
  pendingFrames: PendingFrame[];
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
      const codec = this.config.createCodec(header.codec);
      const frameSize = header.codec === 'codec2' ? 160 : 320;
      state = {
        jitterBuffer: new AdaptiveJitterBuffer({
          minDepth: MIN_BUFFER_DEPTH,
          maxDepth: MAX_BUFFER_DEPTH,
          frameMs: FRAME_MS,
        }),
        codecs: new Map([[header.codec, codec]]),
        speaking: false,
        playbackTimer: null,
        idleTimer: null,
        ready: false,
        pendingFrames: [],
        frameSize,
      };
      this.senders.set(senderHex, state);

      // Capture state reference for stale-closure guard — if the sender is
      // removed and re-created during init, this closure must not operate
      // on the new state (which would install a duplicate playback timer).
      const stateRef = state;
      codec.init(16000, 1).then(() => {
        if (this.senders.get(senderHex) !== stateRef) {
          // Stale closure — sender was removed/recreated during init.
          // init() allocated a new OpusScript on the Emscripten heap;
          // destroy it so it doesn't leak.
          stateRef.codecs.forEach((c) => c.destroy());
          return;
        }
        stateRef.ready = true;
        for (const pf of stateRef.pendingFrames) {
          try {
            const decoder = this.getOrCreateCodec(stateRef, pf.codec);
            const pcm = decoder.decode(pf.payload);
            stateRef.jitterBuffer.insert(pf.sequence, pcm);
          } catch {
            // Drop undecodable frame — jitter buffer will produce silence
          }
        }
        stateRef.pendingFrames = [];
        // Start playback timer only after codec is ready
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
    // re-seeds playSeq from the new sequence number — otherwise the playback
    // timer has advanced playSeq past the new frames during silence.
    if (header.pttActive && !state.speaking && state.ready) {
      state.jitterBuffer.reset();
    }
    state.speaking = header.pttActive;

    if (state.idleTimer) clearTimeout(state.idleTimer);
    state.idleTimer = setTimeout(() => {
      this.removeSender(senderHex);
    }, IDLE_TIMEOUT_MS);

    if (state.ready) {
      try {
        const decoder = this.getOrCreateCodec(state, header.codec);
        const pcm = decoder.decode(encodedPayload);
        state.jitterBuffer.insert(header.sequence, pcm);
      } catch {
        // Drop malformed frame — jitter buffer will produce silence
      }
    } else if (state.pendingFrames.length < MAX_PENDING_FRAMES) {
      state.pendingFrames.push({
        sequence: header.sequence,
        payload: encodedPayload,
        codec: header.codec,
      });
    }
  }

  /**
   * Get or lazily create a decoder for the given codec type within
   * a sender's state.
   */
  private getOrCreateCodec(state: SenderState, codecType: CodecType): VoiceCodec {
    let codec = state.codecs.get(codecType);
    if (!codec) {
      codec = this.config.createCodec(codecType);
      state.codecs.set(codecType, codec);
      // Update frame size if we see a new codec type
      state.frameSize = codecType === 'codec2' ? 160 : 320;
    }
    return codec;
  }

  private advancePlayback(senderHex: string): void {
    const state = this.senders.get(senderHex);
    if (!state) return;
    let pcm = state.jitterBuffer.advance();
    // Generate comfort noise for missing frames (only after fill period)
    if (pcm === null && state.jitterBuffer.isReady()) {
      pcm = generateComfortNoise(state.frameSize, 0.005);
    }
    this.config.onPlayFrame?.(senderHex, pcm);
  }

  private removeSender(senderHex: string): void {
    const state = this.senders.get(senderHex);
    if (!state) return;
    if (state.playbackTimer) clearInterval(state.playbackTimer);
    if (state.idleTimer) clearTimeout(state.idleTimer);
    state.codecs.forEach((c) => c.destroy());
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
