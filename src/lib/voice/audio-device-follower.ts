/**
 * ZEB-359 — shared "follow the audio device preferences while media is live"
 * logic. VoiceSession (community channels) and CallSession (1:1 DM calls) both
 * need identical behavior:
 *
 *  - output pref change → route the live mixer (`setOutputDevice`);
 *  - input pref change → restart ONLY the capture leg (`switchInputDevice`);
 *  - preferred input unplugged → restart so the `ideal` constraint falls back
 *    to the system default; replugged → restart again so it takes back over;
 *  - unrelated `devicechange` events (webcams, other hot-plugs) → no restart.
 *
 * Extracted so the two sessions can't drift (the ZEB-355 lesson applied to the
 * frontend). Restart failures surface via `onMicError` — the sessions map that
 * to their existing mic-error/listen-only handling.
 */

import type { AudioDevicePrefs } from '../audio-device-prefs';
import type { VoiceSender } from './voice-sender';
import type { VoiceMixer } from './voice-mixer';

export interface DeviceFollowerDeps {
  prefs: Pick<
    AudioDevicePrefs,
    'getInput' | 'getOutput' | 'subscribe' | 'listDevices'
  >;
  /** Live sender accessor; null while listen-only (mic blocked). */
  getSender: () => Pick<VoiceSender, 'switchInputDevice'> | null;
  /** Live mixer accessor; null before init / after teardown. */
  getMixer: () => Pick<VoiceMixer, 'setOutputDevice'> | null;
  /** A capture restart failed — session decides (listen-only, banner, log). */
  onMicError: (e: unknown) => void;
}

/** Start following; returns the unfollow function (idempotent). */
export function followAudioDevices(deps: DeviceFollowerDeps): () => void {
  let appliedInput = deps.prefs.getInput();
  let appliedOutput = deps.prefs.getOutput();
  /** Whether the preferred input was present at the last look (null = no pref
   *  or not yet enumerated). Presence EDGES (unplug/replug) trigger restarts;
   *  steady states don't. */
  let inputPresent: boolean | null = null;
  let stopped = false;
  // All reactions serialize on one chain: a slow enumerateDevices or capture
  // restart never overlaps the next event's handling.
  let chain: Promise<void> = Promise.resolve();

  const currentPresence = async (): Promise<boolean | null> => {
    const pref = deps.prefs.getInput();
    if (!pref) return null;
    const { inputs } = await deps.prefs.listDevices();
    return inputs.some((d) => d.deviceId === pref);
  };

  const switchGuarded = async (): Promise<void> => {
    const sender = deps.getSender();
    if (!sender) return;
    try {
      await sender.switchInputDevice();
    } catch (e) {
      // Stale-callback guard (PR #495 R1): a switch settling after unfollow
      // must not invoke onMicError — the session closures read live fields,
      // so a torn-down follower could otherwise null the sender of a
      // freshly-rejoined session.
      if (!stopped) deps.onMicError(e);
    }
  };

  // Baseline presence so the first devicechange can detect an unplug edge —
  // plus a one-time reconcile (PR #495 R1, Qodo): a preferred device that was
  // absent at capture start but replugged before this follower attached would
  // otherwise sit on the OS-default fallback with no future edge to fix it.
  // When the pref is present now, restart the capture leg once so it
  // deterministically reflects the preference; sessions attach the follower
  // while still start-muted (D10), so the restart gap is inaudible.
  chain = chain.then(async () => {
    inputPresent = await currentPresence();
    if (stopped || inputPresent !== true) return;
    // Only reconcile the pref that was current at follow start — a pref that
    // changed since then already has a react() queued behind this baseline,
    // and reconciling here too would restart capture twice.
    if (deps.prefs.getInput() !== appliedInput) return;
    await switchGuarded();
  });

  const react = async (): Promise<void> => {
    if (stopped) return;
    const wantOut = deps.prefs.getOutput();
    if (wantOut !== appliedOutput) {
      appliedOutput = wantOut;
      if (!stopped) await deps.getMixer()?.setOutputDevice(wantOut);
    }
    const wantIn = deps.prefs.getInput();
    const present = await currentPresence();
    if (stopped) return;
    const prefChanged = wantIn !== appliedInput;
    const presenceChanged =
      wantIn !== null && inputPresent !== null && present !== inputPresent;
    appliedInput = wantIn;
    inputPresent = present;
    if (!prefChanged && !presenceChanged) return;
    await switchGuarded();
  };

  const unsubscribe = deps.prefs.subscribe(() => {
    chain = chain.then(react).catch(() => {
      // react() only throws through onMicError paths already handled; keep the
      // chain alive regardless.
    });
  });

  return () => {
    stopped = true;
    unsubscribe();
  };
}
