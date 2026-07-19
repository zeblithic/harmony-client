# ZEB-359 + ZEB-355: voice residue — device selection + reconnect-helper extraction

Branch `zeb-359-355-voice-residue` off `main@faa86d35`. One bundled PR closing both
(last two open children of the ZEB-348 voice epic).

## Verified current state

- **V1** Capture: `AudioCapture.start()` calls `getUserMedia({audio: {sampleRate,
  channelCount: 1, echoCancellation: false}})` — no `deviceId` constraint
  (audio-capture.ts:35). Factory-injection test seam already exists.
- **V2** Playback: `VoiceMixer.init()` builds `new AudioContext({sampleRate: 16000})`
  + playback worklet → `ctx.destination` (voice-mixer.ts:86-101). Output selection
  therefore = `AudioContext.setSinkId()` — Chromium/WebView2 only; **WKWebView
  (macOS) lacks it** → feature-detect, degrade to system default + note.
- **V3** Seams: `VoiceSender.start()` → `capture.start(cb, undefined, undefined, sr)`
  (voice-sender.ts:77). Both sessions construct `new AudioCapture()` /
  `makeMixer` (voice-session.ts:349/324, call-session.ts:383/358); mixer DI type is
  `Pick<VoiceMixer, 'init'|'pushFrame'|'drain'|'setDeafened'|'destroy'>`.
- **V4** Settings UI: `SettingsPanel.svelte` = 7-tab tablist; sections stay mounted
  (hidden, not `{#if}`-swapped). No voice tab exists.
- **V5** Persistence idiom: `device-label-service.ts` localStorage pattern
  (try/catch, non-fatal on quota/SSR).
- **V6** Rust (ZEB-355): TWO byte-identical media reconnect loops in
  `src-tauri/src/event_loop.rs` — channel media ~4934-5118 (emits
  `voice-transport-lost/restored` w/ `{communityId, channelId}`) and DM media
  ~5667-5779 (w/ `{callId}`). Skeleton: `made_progress` flag; on drop →
  warn + emit-lost BEFORE the no-progress sleep; progress resets backoff to the
  5s floor, no-progress sleeps then doubles to 60s cap; re-declare inner loop
  tries immediately, backs off only after a failed re-declare, emits restored on
  success, returns on `closing`. A third loop (voice control, ~5265-5399) shares
  only the backoff arithmetic (declare-at-top shape, no UI events). Four older
  plain-backoff loops = ticket's explicit out-of-scope stretch. MSRV 1.91 ⇒
  `AsyncFnMut` closures available. No existing tests pin the reconnect behavior.

## Design — ZEB-355 (extraction)

New `src-tauri/src/voice_reconnect.rs`:

1. `ProgressBackoff` — pure state (floor 5s, cap 60s):
   - `on_drop(made_progress: bool) -> Option<Duration>`: progress → reset to
     floor, `None` (no sleep); no-progress → `Some(current)` (caller sleeps),
     then double toward cap.
   - `on_redeclare_failure() -> Duration`: `current`, then double toward cap.
   Unit-tested inline (no tokio).
2. `run_media_subscriber(...)` — the shared skeleton, concrete over zenoh types,
   parameterized by: initial subscriber, session (re-declares), sub key, log
   label, `closing`, per-sample `AsyncFnMut(Sample)` handler, `on_lost`/
   `on_restored` `Fn()` emit closures. Exact behavior preserved (emit-before-
   sleep, immediate first re-declare, closing checks in both loops).

Channel + DM media call sites become thin closures around their per-frame
pipelines. The voice-control loop adopts `ProgressBackoff` for its arithmetic
only (kills the third copy; shape untouched). Behavior gate: existing voice
integration suites + clippy; no wire/event changes.

## Design — ZEB-359 (device selection)

1. **`src/lib/audio-device-prefs.ts`** — service + exported singleton:
   `getInput()/getOutput(): string | null` (null = system default),
   `setInput()/setOutput()` (persist localStorage `harmony-voice-devices`,
   notify), `subscribe(cb)` (prefs OR device-set changes), `listDevices()`
   (`enumerateDevices` → `{inputs, outputs}`), `supportsOutputSelection()`
   (`'setSinkId' in AudioContext.prototype`), `devicechange` listener
   re-enumerates + notifies. DI-friendly (navigator/media injectable for tests).
2. **Capture**: `AudioCapture.start(..., deviceId?: string | null)` → constraint
   `deviceId: {ideal}` (never hard-fails; OS-default fallback when device gone —
   the ticket's fallback requirement).
3. **Sender**: `VoiceSenderConfig.inputDeviceId?: () => string | null`, read at
   every capture start; retain `onFrame`; new `switchInputDevice(): Promise<void>`
   — capture-only restart (stop + start with current pref; codec, sequence,
   timestamp untouched — receiver sees a silence gap, same as DTX).
4. **Mixer**: `VoiceMixerConfig.outputDeviceId?: () => string | null`, applied in
   `init()` when supported (rejection → warn + default, non-fatal); new
   `setOutputDevice(id)` live-switch on the running ctx. Session mixer `Pick`s
   gain `'setOutputDevice'`.
5. **Sessions** (voice + call): optional dep `audioDevices` defaulting to the
   singleton; wire getters into sender/mixer construction; subscribe while
   media is up (join→leave / connect→reset): input pref change or selected-input
   unplugged → `sender.switchInputDevice()`; output change → `mixer.setOutputDevice()`.
6. **UI**: `VoiceDeviceSettings.svelte` in a new Settings **Voice** tab —
   input + output `<select>`s with "System default" first, refresh on
   devices-changed, generic fallback names when labels are permission-blank,
   output select disabled + note when `setSinkId` unsupported (macOS WKWebView).

Out of scope: per-call/per-channel overrides, sample-rate/AGC settings, the four
stretch backoff loops, group-call device UI beyond what the shared sessions give.

## Tasks (red-first each)

- **T1** `voice_reconnect.rs` `ProgressBackoff` + unit tests.
- **T2** `run_media_subscriber` + swap channel/DM media call sites; control-loop
  arithmetic adopts `ProgressBackoff`. Gates: fmt, clippy `--all-targets`,
  `scripts/test-select --context task`.
- **T3** `audio-device-prefs.ts` + tests (persistence, subscribe, devicechange,
  filtering, support detection).
- **T4** `AudioCapture` deviceId + `VoiceSender` inputDeviceId/switchInputDevice
  + tests (constraint threading, restart preserves clock/codec).
- **T5** `VoiceMixer` output routing + tests (applied when supported, skipped
  when not, rejection non-fatal, live switch).
- **T6** Session wiring both sessions + tests (construction getters, live
  re-apply, unplug fallback, unsubscribe on teardown).
- **T7** `VoiceDeviceSettings.svelte` + SettingsPanel Voice tab + tests.
- **T8** Gates: `npx vitest run`, `npx tsc --noEmit`, cargo fmt/clippy
  `--all-targets`, `scripts/test-select --context task` (paste summary line);
  full sweep `scripts/test-select --full` pre-PR. PR body: "Closes ZEB-359.
  Closes ZEB-355."
