// src/lib/voice/pcm-capture-processor.ts
//
// AudioWorkletProcessor that accumulates input samples into 320-sample frames
// (20 ms at 16 kHz) and posts each completed frame to the main thread.
//
// This file runs exclusively in the audio worklet global scope.  It cannot be
// unit-tested with jsdom — coverage comes through audio-capture.ts's injected
// factory tests.

const FRAME_SIZE = 320;

class PcmCaptureProcessor extends AudioWorkletProcessor {
  private buffer: Float32Array = new Float32Array(FRAME_SIZE);
  private offset = 0;

  process(inputs: Float32Array[][]): boolean {
    const input = inputs[0]?.[0]; // mono channel 0
    if (!input) return true;

    let pos = 0;
    while (pos < input.length) {
      const remaining = FRAME_SIZE - this.offset;
      const toCopy = Math.min(remaining, input.length - pos);
      this.buffer.set(input.subarray(pos, pos + toCopy), this.offset);
      this.offset += toCopy;
      pos += toCopy;

      if (this.offset === FRAME_SIZE) {
        // Transfer ownership of the buffer to avoid per-frame copy overhead
        // in this real-time audio thread.
        const frame = this.buffer;
        this.port.postMessage(frame, [frame.buffer]);
        this.buffer = new Float32Array(FRAME_SIZE);
        this.offset = 0;
      }
    }
    return true;
  }
}

registerProcessor('pcm-capture-processor', PcmCaptureProcessor);
