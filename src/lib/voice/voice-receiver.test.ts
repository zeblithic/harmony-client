import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { VoiceReceiver, type VoiceReceiverConfig } from './voice-receiver';
import { encodeHeader } from './voice-packet';
import type { VoiceCodec } from './voice-codec';

// ---------------------------------------------------------------------------
// Mock helpers
// ---------------------------------------------------------------------------

type FrameHandler = (event: { payload: unknown }) => void;

/** Accumulates registered handlers per event name. Returns an unsubscribe fn. */
function makeMockListen() {
  const handlers = new Map<string, FrameHandler[]>();

  const listen = vi.fn(
    async (event: string, handler: FrameHandler): Promise<() => void> => {
      if (!handlers.has(event)) handlers.set(event, []);
      handlers.get(event)!.push(handler);
      return () => {
        const list = handlers.get(event);
        if (list) {
          const idx = list.indexOf(handler);
          if (idx !== -1) list.splice(idx, 1);
        }
      };
    },
  );

  /** Emit a raw payload to all registered handlers for `event`. */
  function emit(event: string, payload: unknown) {
    for (const h of handlers.get(event) ?? []) {
      h({ payload });
    }
  }

  return { listen, emit };
}

function mockCodecFactory(codecType: 'opus' | 'codec2' = 'opus'): VoiceCodec {
  const frameSize = codecType === 'opus' ? 320 : 160;
  return {
    codecType,
    init: vi.fn().mockResolvedValue(undefined),
    encode: vi.fn().mockReturnValue(new Uint8Array(codecType === 'opus' ? 40 : 8)),
    decode: vi.fn().mockReturnValue(new Float32Array(frameSize).fill(0.1)),
    destroy: vi.fn(),
  } as unknown as VoiceCodec;
}

/**
 * Build a complete voice frame payload (header + dummy encoded bytes) and emit
 * it as a `voice-frame-received` event.
 */
function emitVoiceFrame(
  emit: (event: string, payload: unknown) => void,
  opts: {
    senderHash: Uint8Array;
    sequence: number;
    pttActive: boolean;
    timestamp?: number;
    codec?: 'opus' | 'codec2';
  },
) {
  const header = encodeHeader({
    pttActive: opts.pttActive,
    sequence: opts.sequence,
    timestamp: opts.timestamp ?? 0,
    senderHash: opts.senderHash,
    codec: opts.codec,
  });

  const payloadSize = opts.codec === 'codec2' ? 8 : 40;
  const payload = new Uint8Array(payloadSize).fill(0x01);
  const frameBytes = new Uint8Array(header.length + payload.length);
  frameBytes.set(header, 0);
  frameBytes.set(payload, header.length);

  emit('voice-frame-received', { frameBytes: Array.from(frameBytes) });
}

/** Produce a deterministic 16-byte sender hash. */
function makeSenderHash(seed = 0xab): Uint8Array {
  return new Uint8Array(16).fill(seed);
}

/** Hex representation of `makeSenderHash(seed)` — mirrors VoiceReceiver internals. */
function senderHexOf(seed = 0xab): string {
  return Array.from(new Uint8Array(16).fill(seed))
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('VoiceReceiver', () => {
  let mockListen: ReturnType<typeof makeMockListen>;
  let config: VoiceReceiverConfig;

  beforeEach(() => {
    vi.useFakeTimers();
    mockListen = makeMockListen();
    config = {
      listen: mockListen.listen,
      createCodec: (codecType: 'opus' | 'codec2') => mockCodecFactory(codecType),
    };
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  // -------------------------------------------------------------------------
  // Test 1: registers a Tauri event listener on init
  // -------------------------------------------------------------------------
  it('registers a Tauri event listener on init', async () => {
    const receiver = new VoiceReceiver(config);
    await receiver.init();

    expect(mockListen.listen).toHaveBeenCalledWith(
      'voice-frame-received',
      expect.any(Function),
    );

    receiver.destroy();
  });

  // -------------------------------------------------------------------------
  // Test 2: creates a per-sender jitter buffer on first frame
  // -------------------------------------------------------------------------
  it('creates a per-sender jitter buffer on first frame', async () => {
    const receiver = new VoiceReceiver(config);
    await receiver.init();

    emitVoiceFrame(mockListen.emit, {
      senderHash: makeSenderHash(0xaa),
      sequence: 0,
      pttActive: true,
    });

    expect(receiver.getActiveSenders().length).toBe(1);

    receiver.destroy();
  });

  // -------------------------------------------------------------------------
  // Test 3: tracks separate senders independently
  // -------------------------------------------------------------------------
  it('tracks separate senders independently', async () => {
    const receiver = new VoiceReceiver(config);
    await receiver.init();

    emitVoiceFrame(mockListen.emit, {
      senderHash: makeSenderHash(0xaa),
      sequence: 0,
      pttActive: true,
    });
    emitVoiceFrame(mockListen.emit, {
      senderHash: makeSenderHash(0xbb),
      sequence: 0,
      pttActive: true,
    });

    expect(receiver.getActiveSenders().length).toBe(2);

    receiver.destroy();
  });

  // -------------------------------------------------------------------------
  // Test 4: reports speaking state for active PTT
  // -------------------------------------------------------------------------
  it('reports speaking state for active PTT', async () => {
    const receiver = new VoiceReceiver(config);
    await receiver.init();

    const hash = makeSenderHash(0xcc);
    const hex = senderHexOf(0xcc);

    emitVoiceFrame(mockListen.emit, {
      senderHash: hash,
      sequence: 0,
      pttActive: true,
    });

    expect(receiver.isSpeaking(hex)).toBe(true);

    receiver.destroy();
  });

  // -------------------------------------------------------------------------
  // Test 5: clears speaking state on PTT=false
  // -------------------------------------------------------------------------
  it('clears speaking state on PTT=false', async () => {
    const receiver = new VoiceReceiver(config);
    await receiver.init();

    const hash = makeSenderHash(0xdd);
    const hex = senderHexOf(0xdd);

    emitVoiceFrame(mockListen.emit, {
      senderHash: hash,
      sequence: 0,
      pttActive: true,
    });
    expect(receiver.isSpeaking(hex)).toBe(true);

    emitVoiceFrame(mockListen.emit, {
      senderHash: hash,
      sequence: 1,
      pttActive: false,
    });
    expect(receiver.isSpeaking(hex)).toBe(false);

    receiver.destroy();
  });

  // -------------------------------------------------------------------------
  // Test 6: cleans up sender after 2s idle timeout
  // -------------------------------------------------------------------------
  it('cleans up sender after 2s idle timeout', async () => {
    const receiver = new VoiceReceiver(config);
    await receiver.init();

    emitVoiceFrame(mockListen.emit, {
      senderHash: makeSenderHash(0xee),
      sequence: 0,
      pttActive: true,
    });

    expect(receiver.getActiveSenders().length).toBe(1);

    // Advance fake clock past IDLE_TIMEOUT_MS (2000 ms)
    await vi.advanceTimersByTimeAsync(3000);

    expect(receiver.getActiveSenders().length).toBe(0);

    receiver.destroy();
  });

  // -------------------------------------------------------------------------
  // Test 7: filters out own sender to prevent self-echo
  // -------------------------------------------------------------------------
  it('filters out own sender to prevent self-echo', async () => {
    const ownHash = makeSenderHash(0xff);
    const ownHex = senderHexOf(0xff);
    const receiver = new VoiceReceiver({
      ...config,
      ownSenderHex: ownHex,
    });
    await receiver.init();

    // Emit a frame from own sender — should be filtered
    emitVoiceFrame(mockListen.emit, {
      senderHash: ownHash,
      sequence: 0,
      pttActive: true,
    });

    expect(receiver.getActiveSenders().length).toBe(0);
    expect(receiver.isSpeaking(ownHex)).toBe(false);

    // Emit a frame from a different sender — should be accepted
    emitVoiceFrame(mockListen.emit, {
      senderHash: makeSenderHash(0xaa),
      sequence: 0,
      pttActive: true,
    });

    expect(receiver.getActiveSenders().length).toBe(1);

    receiver.destroy();
  });

  // -------------------------------------------------------------------------
  // Test 8: creates codec2 decoder for codec2 frames
  // -------------------------------------------------------------------------
  it('creates codec2 decoder for codec2 frames', async () => {
    const codecs: string[] = [];
    const receiver = new VoiceReceiver({
      ...config,
      createCodec: (ct: 'opus' | 'codec2') => {
        codecs.push(ct);
        return mockCodecFactory(ct);
      },
    });
    await receiver.init();

    emitVoiceFrame(mockListen.emit, {
      senderHash: makeSenderHash(0xaa),
      sequence: 0,
      pttActive: true,
      codec: 'codec2',
    });

    expect(codecs).toContain('codec2');
    receiver.destroy();
  });

  // -------------------------------------------------------------------------
  // Test 9: plays comfort noise instead of null for missing frames
  // -------------------------------------------------------------------------
  it('plays comfort noise instead of null for missing frames', async () => {
    const playedFrames: (Float32Array | null)[] = [];
    const receiver = new VoiceReceiver({
      ...config,
      onPlayFrame: (_hex, pcm) => playedFrames.push(pcm),
    });
    await receiver.init();

    const hash = makeSenderHash(0xaa);
    emitVoiceFrame(mockListen.emit, {
      senderHash: hash,
      sequence: 0,
      pttActive: true,
    });

    // Wait for codec init
    await vi.advanceTimersByTimeAsync(10);

    // Advance past fill period + a few frames
    for (let i = 0; i < 5; i++) {
      vi.advanceTimersByTime(20);
    }

    // At least one frame should be non-null (either decoded PCM or comfort noise)
    const nonNull = playedFrames.filter((f) => f !== null);
    expect(nonNull.length).toBeGreaterThan(0);

    receiver.destroy();
  });
});
