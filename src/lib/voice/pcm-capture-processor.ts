// src/lib/voice/pcm-capture-processor.ts
//
// AudioWorkletProcessor that accumulates input samples into 20ms frames
// and posts each completed frame to the main thread.

/// <reference types="audioworklet" />
//
// Frame size is derived from the AudioContext sample rate:
//   16 kHz → 320 samples, 8 kHz → 160 samples.
//
// This file runs exclusively in the audio worklet global scope. It cannot be
// unit-tested with jsdom — coverage comes through audio-capture.ts's injected
// factory tests.

const FRAME_MS = 20;

class PcmCaptureProcessor extends AudioWorkletProcessor {
  private buffer: Float32Array;
  private offset = 0;
  private readonly frameSize: number;

  constructor() {
    super();
    // sampleRate is a global in AudioWorkletGlobalScope
    this.frameSize = Math.round(sampleRate * FRAME_MS / 1000);
    this.buffer = new Float32Array(this.frameSize);
  }

  process(inputs: Float32Array[][]): boolean {
    const input = inputs[0]?.[0];
    if (!input) return true;

    let pos = 0;
    while (pos < input.length) {
      const remaining = this.frameSize - this.offset;
      const toCopy = Math.min(remaining, input.length - pos);
      this.buffer.set(input.subarray(pos, pos + toCopy), this.offset);
      this.offset += toCopy;
      pos += toCopy;

      if (this.offset === this.frameSize) {
        const frame = this.buffer;
        this.port.postMessage(frame, [frame.buffer]);
        this.buffer = new Float32Array(this.frameSize);
        this.offset = 0;
      }
    }
    return true;
  }
}

registerProcessor('pcm-capture-processor', PcmCaptureProcessor);
