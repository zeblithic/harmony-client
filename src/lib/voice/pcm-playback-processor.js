// src/lib/voice/pcm-playback-processor.js
//
// Playback worklet: receives mixed Float32 frames (20ms) on its port and
// plays them out via a ring buffer, decoupling the 20ms producer cadence
// from the 128-sample render quantum. Underrun → silence (no glitches).
//
// Plain JS (not TS): loaded as worklet SOURCE via a `?raw` import and run inside
// AudioWorkletGlobalScope, which executes JavaScript, not TypeScript (ZEB-575).
// It cannot be unit-tested with jsdom — coverage comes through voice-mixer.ts's
// injected factory tests.

class PcmPlaybackProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.ring = new Float32Array(48000); // ~1s at 48k; sized generously
    this.writeIdx = 0;
    this.readIdx = 0;
    this.available = 0;
    this.port.onmessage = (e) => {
      const frame = e.data;
      for (let i = 0; i < frame.length; i++) {
        if (this.available >= this.ring.length) break; // drop on overflow
        this.ring[this.writeIdx] = frame[i];
        this.writeIdx = (this.writeIdx + 1) % this.ring.length;
        this.available++;
      }
    };
  }

  process(_inputs, outputs) {
    const channels = outputs[0];
    const frameLen = channels[0].length;
    for (let i = 0; i < frameLen; i++) {
      let sample = 0;
      if (this.available > 0) {
        sample = this.ring[this.readIdx];
        this.readIdx = (this.readIdx + 1) % this.ring.length;
        this.available--;
      }
      // Mono mix → fan out to every output channel. Writing only channels[0]
      // leaves the right speaker/ear silent on stereo (multi-channel) outputs.
      for (let c = 0; c < channels.length; c++) channels[c][i] = sample;
    }
    return true;
  }
}

registerProcessor('pcm-playback-processor', PcmPlaybackProcessor);
