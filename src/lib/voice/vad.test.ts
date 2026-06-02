import { describe, it, expect } from 'vitest';
import { VoiceActivityDetector } from './vad';

/** Build a 320-sample (20ms @16k) frame of constant amplitude. */
function frame(amp: number, n = 320): Float32Array {
  const f = new Float32Array(n);
  f.fill(amp);
  return f;
}

describe('VoiceActivityDetector', () => {
  it('reports silence below threshold', () => {
    const vad = new VoiceActivityDetector({ threshold: 0.02, hangoverMs: 200, frameMs: 20 });
    expect(vad.process(frame(0.0))).toBe(false);
    expect(vad.process(frame(0.005))).toBe(false);
  });

  it('reports speaking at/above threshold', () => {
    const vad = new VoiceActivityDetector({ threshold: 0.02, hangoverMs: 200, frameMs: 20 });
    expect(vad.process(frame(0.1))).toBe(true);
  });

  it('holds speaking through hangover then drops', () => {
    const vad = new VoiceActivityDetector({ threshold: 0.02, hangoverMs: 200, frameMs: 20 });
    expect(vad.process(frame(0.1))).toBe(true);      // loud → speaking
    // 200ms hangover / 20ms = 10 silent frames still report speaking
    for (let i = 0; i < 10; i++) {
      expect(vad.process(frame(0.0))).toBe(true);
    }
    // 11th silent frame: hangover expired
    expect(vad.process(frame(0.0))).toBe(false);
  });

  it('reset() clears state', () => {
    const vad = new VoiceActivityDetector({ threshold: 0.02, hangoverMs: 200, frameMs: 20 });
    vad.process(frame(0.1));
    vad.reset();
    expect(vad.process(frame(0.0))).toBe(false);
  });
});
