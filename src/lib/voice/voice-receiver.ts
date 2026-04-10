import { JitterBuffer } from './jitter-buffer';
import { decodeHeader, HEADER_SIZE } from './voice-packet';
import type { OpusCodec } from './opus-codec';

const FRAME_MS = 20;
const BUFFER_DEPTH = 4; // 80ms
const IDLE_TIMEOUT_MS = 2000;

interface SenderState {
  jitterBuffer: JitterBuffer;
  codec: OpusCodec;
  speaking: boolean;
  lastFrameTime: number;
  playbackTimer: ReturnType<typeof setInterval> | null;
  idleTimer: ReturnType<typeof setTimeout> | null;
}

export interface VoiceReceiverConfig {
  listen: (
    event: string,
    handler: (event: { payload: unknown }) => void,
  ) => Promise<() => void>;
  createCodec: () => OpusCodec;
  onPlayFrame?: (senderHex: string, pcm: Float32Array | null) => void;
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

    let state = this.senders.get(senderHex);
    if (!state) {
      const codec = this.config.createCodec();
      codec.init(16000, 1); // fire-and-forget
      state = {
        jitterBuffer: new JitterBuffer(BUFFER_DEPTH, FRAME_MS),
        codec,
        speaking: false,
        lastFrameTime: Date.now(),
        playbackTimer: null,
        idleTimer: null,
      };
      this.senders.set(senderHex, state);
      state.playbackTimer = setInterval(() => {
        this.advancePlayback(senderHex);
      }, FRAME_MS);
    }

    state.speaking = header.pttActive;
    state.lastFrameTime = Date.now();

    if (state.idleTimer) clearTimeout(state.idleTimer);
    state.idleTimer = setTimeout(() => {
      this.removeSender(senderHex);
    }, IDLE_TIMEOUT_MS);

    const pcm = state.codec.decode(opusPayload);
    state.jitterBuffer.insert(header.sequence, pcm);
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
    for (const [key] of this.senders) {
      this.removeSender(key);
    }
  }
}
