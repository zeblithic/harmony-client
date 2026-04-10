import { JitterBuffer } from './jitter-buffer';
import { decodeHeader, HEADER_SIZE } from './voice-packet';
import type { OpusCodec } from './opus-codec';

const FRAME_MS = 20;
const BUFFER_DEPTH = 4; // 80ms
const IDLE_TIMEOUT_MS = 2000;
/** Max frames queued during async codec init (~320ms of audio). */
const MAX_PENDING_FRAMES = 16;

interface PendingFrame {
  sequence: number;
  opusPayload: Uint8Array;
}

interface SenderState {
  jitterBuffer: JitterBuffer;
  codec: OpusCodec;
  speaking: boolean;
  playbackTimer: ReturnType<typeof setInterval> | null;
  idleTimer: ReturnType<typeof setTimeout> | null;
  /** False until codec.init() resolves. Frames are queued while false. */
  ready: boolean;
  pendingFrames: PendingFrame[];
}

export interface VoiceReceiverConfig {
  listen: (
    event: string,
    handler: (event: { payload: unknown }) => void,
  ) => Promise<() => void>;
  createCodec: () => OpusCodec;
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
    const opusPayload = bytes.slice(HEADER_SIZE);
    const senderHex = Array.from(header.senderHash)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');

    // Filter out own frames to prevent self-echo
    if (this.config.ownSenderHex && senderHex === this.config.ownSenderHex) return;

    let state = this.senders.get(senderHex);
    if (!state) {
      const codec = this.config.createCodec();
      state = {
        jitterBuffer: new JitterBuffer(BUFFER_DEPTH, FRAME_MS),
        codec,
        speaking: false,
        playbackTimer: null,
        idleTimer: null,
        ready: false,
        pendingFrames: [],
      };
      this.senders.set(senderHex, state);

      // Capture state reference for stale-closure guard — if the sender is
      // removed and re-created during init, this closure must not operate
      // on the new state (which would install a duplicate playback timer).
      const stateRef = state;
      codec.init(16000, 1).then(() => {
        if (this.senders.get(senderHex) !== stateRef) return; // stale
        stateRef.ready = true;
        for (const pf of stateRef.pendingFrames) {
          try {
            const pcm = stateRef.codec.decode(pf.opusPayload);
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
        const pcm = state.codec.decode(opusPayload);
        state.jitterBuffer.insert(header.sequence, pcm);
      } catch {
        // Drop malformed frame — jitter buffer will produce silence
      }
    } else if (state.pendingFrames.length < MAX_PENDING_FRAMES) {
      state.pendingFrames.push({ sequence: header.sequence, opusPayload });
    }
  }

  private advancePlayback(senderHex: string): void {
    const state = this.senders.get(senderHex);
    if (!state) return;
    const pcm = state.jitterBuffer.advance();
    this.config.onPlayFrame?.(senderHex, pcm);
  }

  private removeSender(senderHex: string): void {
    const state = this.senders.get(senderHex);
    if (!state) return;
    if (state.playbackTimer) clearInterval(state.playbackTimer);
    if (state.idleTimer) clearTimeout(state.idleTimer);
    state.codec.destroy();
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
