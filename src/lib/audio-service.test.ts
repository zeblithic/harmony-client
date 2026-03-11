// src/lib/audio-service.test.ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { AudioService } from './audio-service';

function createMockAudioContext() {
  const analyser = { connect: vi.fn(), disconnect: vi.fn() };
  const source = { connect: vi.fn(), disconnect: vi.fn() };
  return {
    createAnalyser: vi.fn().mockReturnValue(analyser),
    createMediaStreamSource: vi.fn().mockReturnValue(source),
    destination: {},
    sampleRate: 48000,
    close: vi.fn().mockResolvedValue(undefined),
    state: 'running' as AudioContextState,
  };
}

function createMockStream() {
  return {
    getTracks: vi.fn().mockReturnValue([{ stop: vi.fn() }]),
  };
}

describe('AudioService', () => {
  let mockCtx: ReturnType<typeof createMockAudioContext>;
  let mockStream: ReturnType<typeof createMockStream>;

  beforeEach(() => {
    mockCtx = createMockAudioContext();
    mockStream = createMockStream();
    // Mock navigator.mediaDevices
    Object.defineProperty(global.navigator, 'mediaDevices', {
      value: {
        getUserMedia: vi.fn().mockResolvedValue(mockStream),
      },
      writable: true,
      configurable: true,
    });
  });

  it('isActive returns false initially', () => {
    const service = new AudioService();
    expect(service.isActive()).toBe(false);
  });

  it('start requests microphone access', async () => {
    const service = new AudioService();
    await service.start(vi.fn(), () => mockCtx as unknown as AudioContext);
    expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledWith({
      audio: { sampleRate: 16000, channelCount: 1, echoCancellation: false },
    });
  });

  it('isActive returns true after start', async () => {
    const service = new AudioService();
    await service.start(vi.fn(), () => mockCtx as unknown as AudioContext);
    expect(service.isActive()).toBe(true);
  });

  it('stop releases resources', async () => {
    const service = new AudioService();
    await service.start(vi.fn(), () => mockCtx as unknown as AudioContext);
    await service.stop();
    expect(service.isActive()).toBe(false);
  });

  it('stop is safe to call when not active', async () => {
    const service = new AudioService();
    await expect(service.stop()).resolves.toBeUndefined();
  });
});
