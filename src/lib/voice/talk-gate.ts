// src/lib/voice/talk-gate.ts
//
// Pure per-frame talk gate shared by the channel voice controller
// (VoiceSession) and the 1:1 DM call controller (CallSession). Extracting it
// keeps the mute / PTT / VAD send-decision logic in one place so the DM
// controller can't silently re-introduce bugs the channel controller already
// fixed.

import { VoiceActivityDetector } from './vad';

export interface TalkState {
  muted: boolean;
  pttMode: boolean;
  pttHeld: boolean;
}

/**
 * Build a pure per-frame talk gate.
 *
 * Decision precedence (matches VoiceSession's original inline gate):
 *  - muted        ⇒ never send; reset the VAD hangover so a later unmute starts
 *                   from silence rather than mid-hangover.
 *  - pttMode      ⇒ follow the hold state; the VAD is ignored entirely.
 *  - open-mic     ⇒ energy-threshold VAD with hangover (state lives in the VAD).
 *
 * Returns `{ send, ptt }`; in every non-PTT branch `ptt === send` (the PTT
 * header bit tracks "is this an active speech frame"), and in PTT mode both
 * follow the hold.
 */
export function makeTalkGate(
  getState: () => TalkState,
  vad: VoiceActivityDetector,
): (pcm: Float32Array) => { send: boolean; ptt: boolean } {
  return (pcm) => {
    const { muted, pttMode, pttHeld } = getState();
    if (muted) {
      vad.reset();
      return { send: false, ptt: false };
    }
    if (pttMode) return { send: pttHeld, ptt: pttHeld };
    const active = vad.process(pcm);
    return { send: active, ptt: active };
  };
}
