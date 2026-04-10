// src/lib/voice/audio-capture.ts
//
// Real AudioWorklet-based PCM capture service.  Replaces the old
// audio-service.ts AnalyserNode placeholder.
//
// The class accepts optional factory functions for AudioContext and
// AudioWorkletNode so that tests can inject mocks without touching jsdom's
// (non-existent) Web Audio implementation.

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
  ): Promise<void> {
    if (this.active) return;

    this.stream = await navigator.mediaDevices.getUserMedia({
      audio: { sampleRate: 16000, channelCount: 1, echoCancellation: false },
    });

    this.context = createContext
      ? createContext()
      : new AudioContext({ sampleRate: 16000 });

    if (!createWorkletNode) {
      await this.context.audioWorklet.addModule(
        new URL('./pcm-capture-processor.ts', import.meta.url).href,
      );
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
  }

  async stop(): Promise<void> {
    if (!this.active) return;
    this.worklet?.disconnect();
    this.source?.disconnect();
    this.stream?.getTracks().forEach(t => t.stop());
    await this.context?.close();
    this.worklet = null;
    this.source = null;
    this.stream = null;
    this.context = null;
    this.active = false;
  }
}
