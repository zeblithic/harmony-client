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

  // Baseline presence so the first devicechange can detect an unplug edge.
  chain = chain.then(async () => {
    inputPresent = await currentPresence();
  });

  const react = async (): Promise<void> => {
    if (stopped) return;
    const wantOut = deps.prefs.getOutput();
    if (wantOut !== appliedOutput) {
      appliedOutput = wantOut;
      await deps.getMixer()?.setOutputDevice(wantOut);
    }
    const wantIn = deps.prefs.getInput();
    const present = await currentPresence();
    const prefChanged = wantIn !== appliedInput;
    const presenceChanged =
      wantIn !== null && inputPresent !== null && present !== inputPresent;
    appliedInput = wantIn;
    inputPresent = present;
    if (!prefChanged && !presenceChanged) return;
    const sender = deps.getSender();
    if (!sender) return;
    try {
      await sender.switchInputDevice();
    } catch (e) {
      deps.onMicError(e);
    }
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
