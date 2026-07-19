// src/lib/voice/audio-capture.ts
//
// Real AudioWorklet-based PCM capture service.  Replaces the old
// audio-service.ts AnalyserNode placeholder.
//
// The class accepts optional factory functions for AudioContext and
// AudioWorkletNode so that tests can inject mocks without touching jsdom's
// (non-existent) Web Audio implementation.

import { loadWorkletModule } from './load-worklet-module';
// `?raw` inlines the plain-JS worklet source as a string; loadWorkletModule
// loads it from a same-origin blob: URL (ZEB-575).
import captureProcessorSource from './pcm-capture-processor.js?raw';

export type FrameCallback = (pcm: Float32Array) => void;

export class AudioCapture {
  private stream: MediaStream | null = null;
  private context: AudioContext | null = null;
  private source: MediaStreamAudioSourceNode | null = null;
  private worklet: AudioWorkletNode | null = null;
  private active = false;

  isActive(): boolean { return this.active; }

  async start(
    onFrame: FrameCallback,
    createContext?: () => AudioContext,
    createWorkletNode?: (ctx: AudioContext) => AudioWorkletNode,
    sampleRate = 16000,
    deviceId: string | null = null,
  ): Promise<void> {
    if (this.active) return;

    try {
      // ZEB-359: `ideal` (not `exact`) so an unplugged preferred device falls
      // back to the OS default instead of failing the whole capture.
      const audio: MediaTrackConstraints = {
        sampleRate,
        channelCount: 1,
        echoCancellation: false,
        ...(deviceId ? { deviceId: { ideal: deviceId } } : {}),
      };
      this.stream = await navigator.mediaDevices.getUserMedia({ audio });

      this.context = createContext
        ? createContext()
        : new AudioContext({ sampleRate });

      if (!createWorkletNode) {
        await loadWorkletModule(this.context, captureProcessorSource);
      }

      this.source = this.context.createMediaStreamSource(this.stream);

      this.worklet = createWorkletNode
        ? createWorkletNode(this.context)
        : new AudioWorkletNode(this.context, 'pcm-capture-processor');

      this.worklet.port.onmessage = (e: MessageEvent) => {
        onFrame(e.data as Float32Array);
      };

      this.source.connect(this.worklet);
      this.active = true;
    } catch (err) {
      // Clean up partially-created resources on failure
      this.worklet?.disconnect();
      this.source?.disconnect();
      this.stream?.getTracks().forEach(t => t.stop());
      if (this.context) {
        await this.context.close().catch(() => {});
      }
      this.worklet = null;
      this.source = null;
      this.stream = null;
      this.context = null;
      this.active = false;
      throw err;
    }
  }

  async stop(): Promise<void> {
    if (!this.active) return;
    this.worklet?.disconnect();
    this.source?.disconnect();
    this.stream?.getTracks().forEach(t => t.stop());
    // AudioContext.close() can reject (double-close races, already-closed state,
    // policy errors). Swallow it — the rest of this cleanup still matters, and
    // the caller has no useful recovery besides "move on". Mirrors the
    // defensive .catch(() => {}) in start()'s own cleanup path above.
    await this.context?.close().catch(() => {});
    this.worklet = null;
    this.source = null;
    this.stream = null;
    this.context = null;
    this.active = false;
  }
}
