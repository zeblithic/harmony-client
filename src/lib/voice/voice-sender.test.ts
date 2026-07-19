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
    communityId: 'a'.repeat(32),
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
    expect(payload.communityId).toBe('a'.repeat(32));
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

  it('skips frames the gate rejects and uses the gate ptt flag', async () => {
    const invoke = vi.fn().mockResolvedValue(undefined);
    let onFrame!: (pcm: Float32Array) => void;
    const capture = {
      start: vi.fn(async (cb: (pcm: Float32Array) => void) => { onFrame = cb; }),
      stop: vi.fn(async () => {}),
      isActive: () => true,
    };
    const codec = {
      init: vi.fn(async () => {}), encode: () => new Uint8Array([1, 2, 3]),
      decode: () => new Float32Array(0), destroy: vi.fn(), codecType: 'opus' as const,
    };
    let allow = false;
    const sender = new VoiceSender({
      senderHash: new Uint8Array(16), communityId: 'c', channelId: 'ch',
      invoke, codec, capture: capture as never,
      frameGate: () => ({ send: allow, ptt: allow }),
    });
    await sender.start();
    onFrame(new Float32Array(320));          // gate rejects
    expect(invoke).not.toHaveBeenCalled();
    allow = true;
    onFrame(new Float32Array(320));          // gate accepts
    expect(invoke).toHaveBeenCalledTimes(1);
    const arg = invoke.mock.calls[0][1] as { payload: { frameBytes: number[] } };
    // ptt bit lives in header byte 0 bit 3 (0x08)
    expect((arg.payload.frameBytes[0] & 0x08) !== 0).toBe(true);
  });

  it('advances sequence on gated (DTX) frames so resumed audio stays in sync', async () => {
    // Regression (CodeAnt): a VAD/DTX gap drops frames but the receiver playhead
    // keeps advancing. If the sender froze its sequence across the gap, resumed
    // frames would arrive "behind" the playhead and be discarded as late. The
    // stream clock must tick on every CAPTURED frame, not just transmitted ones.
    const invoke = vi.fn().mockResolvedValue(undefined);
    let onFrame!: (pcm: Float32Array) => void;
    const capture = {
      start: vi.fn(async (cb: (pcm: Float32Array) => void) => { onFrame = cb; }),
      stop: vi.fn(async () => {}),
      isActive: () => true,
    };
    const codec = {
      init: vi.fn(async () => {}), encode: () => new Uint8Array([1, 2, 3]),
      decode: () => new Float32Array(0), destroy: vi.fn(), codecType: 'opus' as const,
    };
    let allow = true;
    const sender = new VoiceSender({
      senderHash: new Uint8Array(16), communityId: 'c', channelId: 'ch',
      invoke, codec, capture: capture as never,
      frameGate: () => ({ send: allow, ptt: allow }),
    });
    await sender.start();
    const pcm = new Float32Array(320);
    onFrame(pcm);                       // voiced → sent, seq 0
    allow = false;
    onFrame(pcm); onFrame(pcm); onFrame(pcm); // 3 gated (DTX) frames — not sent
    allow = true;
    onFrame(pcm);                       // resumed → sent

    expect(invoke).toHaveBeenCalledTimes(2); // only the 2 voiced frames left the box
    const seqOf = (i: number) => {
      const payload = (invoke.mock.calls[i][1] as { payload: { frameBytes: number[] } }).payload;
      return decodeHeader(new Uint8Array(payload.frameBytes.slice(0, HEADER_SIZE))).sequence;
    };
    expect(seqOf(0)).toBe(0);
    // The 3 silent frames advanced the clock, so the resumed frame is seq 4 —
    // matching the receiver playhead that advanced through the gap. Pre-fix it
    // would have been seq 1 and been dropped as "late".
    expect(seqOf(1)).toBe(4);
  });

  // ZEB-359: preferred-input threading + live switch.
  describe('input device selection', () => {
    it('start passes the current inputDeviceId to capture.start', async () => {
      const sender = new VoiceSender({
        ...ctx.config,
        inputDeviceId: () => 'mic-7',
      });
      await sender.start();
      expect(ctx.mockCapture.start).toHaveBeenCalledWith(
        expect.any(Function),
        undefined,
        undefined,
        16000,
        'mic-7',
      );
    });

    it('start passes null (system default) when no provider is configured', async () => {
      const sender = new VoiceSender(ctx.config);
      await sender.start();
      expect(ctx.mockCapture.start).toHaveBeenCalledWith(
        expect.any(Function),
        undefined,
        undefined,
        16000,
        null,
      );
    });

    it('switchInputDevice restarts capture with the new pref, preserving codec and clock', async () => {
      let pref: string | null = 'mic-a';
      const sender = new VoiceSender({
        ...ctx.config,
        inputDeviceId: () => pref,
      });
      await sender.start();
      const before = ctx.getCapturedOnFrame()!;
      before(new Float32Array(320)); // seq 0

      pref = 'mic-b';
      await sender.switchInputDevice();

      expect(ctx.mockCapture.stop).toHaveBeenCalledTimes(1);
      expect(ctx.mockCapture.start).toHaveBeenCalledTimes(2);
      expect(
        (ctx.mockCapture.start as ReturnType<typeof vi.fn>).mock.calls[1][4],
      ).toBe('mic-b');
      // Codec survives the switch (no re-init, no destroy)…
      expect(ctx.mockCodec.destroy).not.toHaveBeenCalled();
      expect(ctx.mockCodec.init).toHaveBeenCalledTimes(1);
      // …and the stream clock continues: the next frame is seq 1, not 0.
      const after = ctx.getCapturedOnFrame()!;
      after(new Float32Array(320));
      const last = ctx.mockInvoke.mock.calls.at(-1)!;
      const payload = (last[1] as Record<string, unknown>).payload as {
        frameBytes: number[];
      };
      const header = new Uint8Array(payload.frameBytes.slice(0, HEADER_SIZE));
      expect(decodeHeader(header).sequence).toBe(1);
    });

    it('switchInputDevice is a no-op while the sender is not active', async () => {
      const sender = new VoiceSender(ctx.config);
      await sender.switchInputDevice();
      expect(ctx.mockCapture.stop).not.toHaveBeenCalled();
      expect(ctx.mockCapture.start).not.toHaveBeenCalled();
    });

    it('a failed capture restart deactivates the sender and surfaces the error', async () => {
      const sender = new VoiceSender({
        ...ctx.config,
        inputDeviceId: () => 'mic-a',
      });
      await sender.start();
      (ctx.mockCapture.start as ReturnType<typeof vi.fn>).mockRejectedValueOnce(
        new Error('mic gone'),
      );
      await expect(sender.switchInputDevice()).rejects.toThrow('mic gone');
      // The sender must not pretend audio still flows: codec released, and a
      // subsequent start() fully re-initializes.
      expect(ctx.mockCodec.destroy).toHaveBeenCalledTimes(1);
      await sender.start();
      expect(ctx.mockCodec.init).toHaveBeenCalledTimes(2);
    });
  });
});
