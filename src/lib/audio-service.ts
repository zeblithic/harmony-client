// src/lib/audio-service.ts

export type PcmChunkCallback = (pcm: Float32Array) => void;

export class AudioService {
  private stream: MediaStream | null = null;
  private context: AudioContext | null = null;
  private source: MediaStreamAudioSourceNode | null = null;
  private active = false;

  isActive(): boolean {
    return this.active;
  }

  async start(
    onChunk: PcmChunkCallback,
    createContext?: () => AudioContext,
  ): Promise<void> {
    if (this.active) return;

    this.stream = await navigator.mediaDevices.getUserMedia({
      audio: { sampleRate: 16000, channelCount: 1, echoCancellation: false },
    });

    this.context = createContext
      ? createContext()
      : new AudioContext({ sampleRate: 16000 });

    this.source = this.context.createMediaStreamSource(this.stream);

    // In production, connect to an AudioWorklet for 16kHz capture.
    // For now, connect to an analyser as a placeholder.
    const analyser = this.context.createAnalyser();
    this.source.connect(analyser);

    this.active = true;
  }

  stop(): void {
    if (!this.active) return;

    this.source?.disconnect();
    this.stream?.getTracks().forEach(t => t.stop());
    this.context?.close();

    this.source = null;
    this.stream = null;
    this.context = null;
    this.active = false;
  }
}
