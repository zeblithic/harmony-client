import { describe, it, expect } from 'vitest';
import { VoiceActivityDetector } from './vad';
import { makeTalkGate, type TalkState } from './talk-gate';

function gateWith(state: TalkState, threshold = 0.02) {
  const s = { ...state };
  const vad = new VoiceActivityDetector({ threshold });
  const gate = makeTalkGate(() => s, vad);
  return { gate, set: (p: Partial<TalkState>) => Object.assign(s, p) };
}

const loud = () => new Float32Array(320).fill(0.2);
const quiet = () => new Float32Array(320);

describe('makeTalkGate', () => {
  it('muted ⇒ never sends, regardless of energy', () => {
    const { gate } = gateWith({ muted: true, pttMode: false, pttHeld: false });
    expect(gate(loud())).toEqual({ send: false, ptt: false });
    expect(gate(quiet())).toEqual({ send: false, ptt: false });
  });

  it('open-mic: follows VAD energy, then drops after hangover on silence', () => {
    const { gate } = gateWith({ muted: false, pttMode: false, pttHeld: false });
    // Loud frame opens the gate (arms a ~200ms / 20ms = 10-frame hangover).
    expect(gate(loud()).send).toBe(true);
    // Hangover keeps it open across the next 10 silent frames...
    for (let i = 0; i < 10; i++) expect(gate(quiet()).send).toBe(true);
    // ...then it closes once the hangover is exhausted.
    expect(gate(quiet()).send).toBe(false);
  });

  it('PTT mode ignores VAD and follows the hold', () => {
    const { gate, set } = gateWith({ muted: false, pttMode: true, pttHeld: false });
    // Not held ⇒ silent even on a loud frame.
    expect(gate(loud())).toEqual({ send: false, ptt: false });
    // Held ⇒ sends even on a quiet frame (VAD ignored).
    set({ pttHeld: true });
    expect(gate(quiet())).toEqual({ send: true, ptt: true });
    // Release ⇒ silent again immediately (no hangover in PTT mode).
    set({ pttHeld: false });
    expect(gate(loud())).toEqual({ send: false, ptt: false });
  });

  it('muting resets the VAD hangover so a later unmute starts from silence', () => {
    const { gate, set } = gateWith({ muted: false, pttMode: false, pttHeld: false });
    // Open the gate (arms the hangover counter).
    expect(gate(loud()).send).toBe(true);
    // Mute: should reset the VAD, returning silent.
    set({ muted: true });
    expect(gate(quiet()).send).toBe(false);
    // Unmute on a quiet frame: the hangover was reset, so it must be silent
    // (a stale hangover would have leaked a spurious "send").
    set({ muted: false });
    expect(gate(quiet()).send).toBe(false);
  });

  it('ptt bit mirrors send in open-mic mode', () => {
    const { gate } = gateWith({ muted: false, pttMode: false, pttHeld: false });
    const d = gate(loud());
    expect(d.ptt).toBe(d.send);
    expect(d.ptt).toBe(true);
  });
});
