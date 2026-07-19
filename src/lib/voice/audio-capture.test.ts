// src/lib/voice/audio-capture.test.ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { AudioCapture } from './audio-capture';

function createMockAudioContext() {
  const workletNode = {
    port: { onmessage: null as ((e: MessageEvent) => void) | null },
    connect: vi.fn(),
    disconnect: vi.fn(),
  };
  const source = { connect: vi.fn(), disconnect: vi.fn() };
  return {
    ctx: {
      createMediaStreamSource: vi.fn().mockReturnValue(source),
      audioWorklet: { addModule: vi.fn().mockResolvedValue(undefined) },
      close: vi.fn().mockResolvedValue(undefined),
      sampleRate: 16000,
      destination: {},
    },
    workletNode,
    source,
  };
}

function createMockStream() {
  return { getTracks: vi.fn().mockReturnValue([{ stop: vi.fn() }]) };
}

describe('AudioCapture', () => {
  let mocks: ReturnType<typeof createMockAudioContext>;
  let mockStream: ReturnType<typeof createMockStream>;

  beforeEach(() => {
    mocks = createMockAudioContext();
    mockStream = createMockStream();
    Object.defineProperty(global.navigator, 'mediaDevices', {
      value: {
        getUserMedia: vi.fn().mockResolvedValue(mockStream),
      },
      writable: true,
      configurable: true,
    });
  });

  it('is not active initially', () => {
    const capture = new AudioCapture();
    expect(capture.isActive()).toBe(false);
  });

  it('start requests microphone at 16kHz mono', async () => {
    const capture = new AudioCapture();
    await capture.start(
      vi.fn(),
      () => mocks.ctx as unknown as AudioContext,
      () => mocks.workletNode as unknown as AudioWorkletNode,
    );
    expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledWith({
      audio: { sampleRate: 16000, channelCount: 1, echoCancellation: false },
    });
  });

  it('start passes sampleRate to getUserMedia', async () => {
    const capture = new AudioCapture();
    await capture.start(
      vi.fn(),
      () => mocks.ctx as unknown as AudioContext,
      () => mocks.workletNode as unknown as AudioWorkletNode,
      8000,
    );
    expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledWith({
      audio: { sampleRate: 8000, channelCount: 1, echoCancellation: false },
    });
  });

  it('stop releases resources', async () => {
    const capture = new AudioCapture();
    await capture.start(
      vi.fn(),
      () => mocks.ctx as unknown as AudioContext,
      () => mocks.workletNode as unknown as AudioWorkletNode,
    );
    await capture.stop();
    expect(capture.isActive()).toBe(false);
    expect(mocks.workletNode.disconnect).toHaveBeenCalled();
    expect(mocks.source.disconnect).toHaveBeenCalled();
    const tracks = mockStream.getTracks();
    expect(tracks[0].stop).toHaveBeenCalled();
    expect(mocks.ctx.close).toHaveBeenCalled();
  });

  it('fires onFrame when worklet posts a message', async () => {
    const capture = new AudioCapture();
    const onFrame = vi.fn();
    await capture.start(
      onFrame,
      () => mocks.ctx as unknown as AudioContext,
      () => mocks.workletNode as unknown as AudioWorkletNode,
    );

    const pcm = new Float32Array(320);
    pcm.fill(0.5);
    const event = new MessageEvent('message', { data: pcm });
    mocks.workletNode.port.onmessage!(event);

    expect(onFrame).toHaveBeenCalledTimes(1);
    expect(onFrame).toHaveBeenCalledWith(pcm);
  });

  it('start is idempotent', async () => {
    const capture = new AudioCapture();
    const factory = vi.fn().mockReturnValue(mocks.workletNode as unknown as AudioWorkletNode);
    await capture.start(
      vi.fn(),
      () => mocks.ctx as unknown as AudioContext,
      factory,
    );
    await capture.start(
      vi.fn(),
      () => mocks.ctx as unknown as AudioContext,
      factory,
    );
    // getUserMedia should only be called once
    expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledTimes(1);
    // The worklet node factory should only be called once
    expect(factory).toHaveBeenCalledTimes(1);
  });

  it('stop is safe to call when not active', async () => {
    const capture = new AudioCapture();
    await expect(capture.stop()).resolves.toBeUndefined();
  });

  // ZEB-359: preferred-input threading.
  it('start passes a preferred deviceId as an ideal constraint', async () => {
    const capture = new AudioCapture();
    await capture.start(
      vi.fn(),
      () => mocks.ctx as unknown as AudioContext,
      () => mocks.workletNode as unknown as AudioWorkletNode,
      16000,
      'mic-42',
    );
    expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledWith({
      audio: {
        sampleRate: 16000,
        channelCount: 1,
        echoCancellation: false,
        deviceId: { ideal: 'mic-42' },
      },
    });
  });

  it('start omits the deviceId constraint for the system default (null)', async () => {
    const capture = new AudioCapture();
    await capture.start(
      vi.fn(),
      () => mocks.ctx as unknown as AudioContext,
      () => mocks.workletNode as unknown as AudioWorkletNode,
      16000,
      null,
    );
    expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledWith({
      audio: { sampleRate: 16000, channelCount: 1, echoCancellation: false },
    });
  });
});
