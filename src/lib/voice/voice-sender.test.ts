import { describe, it, expect, vi, beforeEach } from 'vitest';
import { VoiceSender, type VoiceSenderConfig } from './voice-sender';
import { decodeHeader, HEADER_SIZE } from './voice-packet';
import type { VoiceCodec } from './voice-codec';
import type { AudioCapture } from './audio-capture';

// ---------------------------------------------------------------------------
// Mock factories
// ---------------------------------------------------------------------------

function makeSenderHash(): Uint8Array {
  const hash = new Uint8Array(16);
  for (let i = 0; i < 16; i++) hash[i] = 0x10 + i;
  return hash;
}

function makeConfig(): {
  config: VoiceSenderConfig;
  mockInvoke: ReturnType<typeof vi.fn>;
  mockEncode: ReturnType<typeof vi.fn>;
  mockCodec: VoiceCodec;
  mockCapture: AudioCapture;
  getCapturedOnFrame: () => ((pcm: Float32Array) => void) | undefined;
} {
  const mockInvoke = vi.fn().mockResolvedValue(undefined);
  const mockEncode = vi.fn((_pcm: Float32Array) => new Uint8Array(40));

  const mockCodec = {
    codecType: 'opus' as const,
    init: vi.fn().mockResolvedValue(undefined),
    encode: mockEncode,
    decode: vi.fn(),
    destroy: vi.fn(),
  } as unknown as VoiceCodec;

  let capturedOnFrame: ((pcm: Float32Array) => void) | undefined;

  const mockCapture = {
    start: vi.fn(async (onFrame: (pcm: Float32Array) => void) => {
      capturedOnFrame = onFrame;
    }),
    stop: vi.fn().mockResolvedValue(undefined),
    isActive: vi.fn().mockReturnValue(true),
  } as unknown as AudioCapture;

  const config: VoiceSenderConfig = {
    senderHash: makeSenderHash(),
    channelId: 'chan-abc',
    invoke: mockInvoke,
    codec: mockCodec,
    capture: mockCapture,
  };

  return {
    config,
    mockInvoke,
    mockEncode,
    mockCodec,
    mockCapture,
    getCapturedOnFrame: () => capturedOnFrame,
  };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('VoiceSender', () => {
  let ctx: ReturnType<typeof makeConfig>;

  beforeEach(() => {
    ctx = makeConfig();
  });

  it('start initializes codec and capture', async () => {
    const sender = new VoiceSender(ctx.config);
    await sender.start();

    expect((ctx.mockCodec.init as ReturnType<typeof vi.fn>)).toHaveBeenCalledWith(16000, 1);
    expect((ctx.mockCapture.start as ReturnType<typeof vi.fn>)).toHaveBeenCalledTimes(1);
  });

  it('sends voice frames via Tauri invoke', async () => {
    const sender = new VoiceSender(ctx.config);
    await sender.start();

    const onFrame = ctx.getCapturedOnFrame();
    expect(onFrame).toBeDefined();

    const pcm = new Float32Array(320).fill(0.1);
    onFrame!(pcm);

    expect(ctx.mockInvoke).toHaveBeenCalledTimes(1);
    const [cmd, args] = ctx.mockInvoke.mock.calls[0] as [string, Record<string, unknown>];
    expect(cmd).toBe('send_voice_frame');

    // Tauri v2 wraps in parameter name — Rust param is `payload`
    const payload = args.payload as Record<string, unknown>;
    expect(payload.channelId).toBe('chan-abc');

    // frameBytes must be an array
    const frameBytes = payload.frameBytes as number[];
    expect(Array.isArray(frameBytes)).toBe(true);

    // Total length: header (23) + opus payload (40)
    expect(frameBytes.length).toBe(HEADER_SIZE + 40);

    // Decode the header to verify content
    const headerBuf = new Uint8Array(frameBytes.slice(0, HEADER_SIZE));
    const decoded = decodeHeader(headerBuf);
    expect(decoded.pttActive).toBe(true);
    expect(decoded.sequence).toBe(0);
  });

  it('increments sequence number per frame', async () => {
    const sender = new VoiceSender(ctx.config);
    await sender.start();

    const onFrame = ctx.getCapturedOnFrame()!;
    const pcm = new Float32Array(320);

    onFrame(pcm);
    onFrame(pcm);
    onFrame(pcm);

    expect(ctx.mockInvoke).toHaveBeenCalledTimes(3);

    const sequences = ctx.mockInvoke.mock.calls.map(([, args]) => {
      const payload = (args as Record<string, unknown>).payload as Record<string, unknown>;
      const frameBytes = payload.frameBytes as number[];
      const headerBuf = new Uint8Array(frameBytes.slice(0, HEADER_SIZE));
      return decodeHeader(headerBuf).sequence;
    });

    expect(sequences).toEqual([0, 1, 2]);
  });

  it('advances timestamp by 20ms per frame', async () => {
    const sender = new VoiceSender(ctx.config);
    await sender.start();

    const onFrame = ctx.getCapturedOnFrame()!;
    const pcm = new Float32Array(320);

    onFrame(pcm);
    onFrame(pcm);

    const timestamps = ctx.mockInvoke.mock.calls.map(([, args]) => {
      const payload = (args as Record<string, unknown>).payload as Record<string, unknown>;
      const frameBytes = payload.frameBytes as number[];
      const headerBuf = new Uint8Array(frameBytes.slice(0, HEADER_SIZE));
      return decodeHeader(headerBuf).timestamp;
    });

    expect(timestamps[1] - timestamps[0]).toBe(20);
  });

  it('stop sends tail frames with PTT=false', async () => {
    const sender = new VoiceSender(ctx.config);
    await sender.start();

    const onFrame = ctx.getCapturedOnFrame()!;
    const pcm = new Float32Array(320).fill(0.5);

    // Send one active frame
    onFrame(pcm);

    // Stop — should send 3 tail frames with PTT=false
    await sender.stop();

    // Total calls: 1 active + 3 tail
    expect(ctx.mockInvoke.mock.calls.length).toBe(4);

    // Count frames with PTT=false
    const pttFalseCount = ctx.mockInvoke.mock.calls.filter(([, args]) => {
      const payload = (args as Record<string, unknown>).payload as Record<string, unknown>;
      const frameBytes = payload.frameBytes as number[];
      const headerBuf = new Uint8Array(frameBytes.slice(0, HEADER_SIZE));
      return !decodeHeader(headerBuf).pttActive;
    }).length;

    expect(pttFalseCount).toBe(3);
  });

  it('persists sequence across PTT sessions (no reset on re-start)', async () => {
    const sender = new VoiceSender(ctx.config);
    await sender.start();

    const onFrame = ctx.getCapturedOnFrame()!;
    const pcm = new Float32Array(320);

    // Send 2 active frames: seq 0, 1
    onFrame(pcm);
    onFrame(pcm);

    // Stop: sends 3 tail frames at seq 2, 3, 4
    await sender.stop();

    // Re-start — sequence should NOT reset to 0
    await sender.start();
    const onFrame2 = ctx.getCapturedOnFrame()!;
    onFrame2(pcm);

    // Total: 2 active + 3 tail + 1 new = 6
    expect(ctx.mockInvoke.mock.calls.length).toBe(6);

    // Last frame should have sequence 5 (continuing from previous session)
    const lastCall = ctx.mockInvoke.mock.calls[5];
    const payload = (lastCall[1] as Record<string, unknown>).payload as Record<string, unknown>;
    const frameBytes = payload.frameBytes as number[];
    const headerBuf = new Uint8Array(frameBytes.slice(0, HEADER_SIZE));
    expect(decodeHeader(headerBuf).sequence).toBe(5);
  });

  it('sets codec bit to 0 for opus', async () => {
    const sender = new VoiceSender(ctx.config);
    await sender.start();

    const onFrame = ctx.getCapturedOnFrame()!;
    onFrame(new Float32Array(320));

    const [, args] = ctx.mockInvoke.mock.calls[0] as [string, Record<string, unknown>];
    const payload = args.payload as Record<string, unknown>;
    const frameBytes = payload.frameBytes as number[];
    const headerBuf = new Uint8Array(frameBytes.slice(0, HEADER_SIZE));
    const decoded = decodeHeader(headerBuf);
    expect(decoded.codec).toBe('opus');
  });

  it('sets codec bit to 1 for codec2', async () => {
    const codec2Mock = {
      codecType: 'codec2' as const,
      init: vi.fn().mockResolvedValue(undefined),
      encode: vi.fn((_pcm: Float32Array) => new Uint8Array(8)),
      decode: vi.fn(),
      destroy: vi.fn(),
    } as unknown as VoiceCodec;

    const config: VoiceSenderConfig = {
      ...ctx.config,
      codec: codec2Mock,
      sampleRate: 8000,
    };
    const sender = new VoiceSender(config);
    await sender.start();

    const onFrame = ctx.getCapturedOnFrame()!;
    onFrame(new Float32Array(160));

    const [, args] = ctx.mockInvoke.mock.calls[0] as [string, Record<string, unknown>];
    const payload = args.payload as Record<string, unknown>;
    const frameBytes = payload.frameBytes as number[];
    const headerBuf = new Uint8Array(frameBytes.slice(0, HEADER_SIZE));
    const decoded = decodeHeader(headerBuf);
    expect(decoded.codec).toBe('codec2');
  });

  it('uses configured sampleRate for codec init', async () => {
    const config: VoiceSenderConfig = {
      ...ctx.config,
      sampleRate: 8000,
    };
    const sender = new VoiceSender(config);
    await sender.start();

    expect((ctx.mockCodec.init as ReturnType<typeof vi.fn>)).toHaveBeenCalledWith(8000, 1);
  });
});
