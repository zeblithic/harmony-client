<script lang="ts">
  import type { Stq8ServiceLike } from '../stq8-service';
  import { AudioCapture } from '../voice/audio-capture';
  import * as profileStorage from '../stq8-profile-storage';

  // Syllable order matches Syllable::from_nibble in stq8-core:
  //   0='O, 1='U, 2='E, 3='I, 4=JO, 5=JU, ..., 15=VI
  // The `name` is Q8-FLAT phonetic text (what the user says aloud); `hint`
  // spells out the consonant for unfamiliar symbols.
  const SYLLABLES: ReadonlyArray<{ name: string; hint: string }> = [
    { name: "'O", hint: 'glottal stop + O' },
    { name: "'U", hint: 'glottal stop + U' },
    { name: "'E", hint: 'glottal stop + E' },
    { name: "'I", hint: 'glottal stop + I' },
    { name: 'JO', hint: 'J + O' },
    { name: 'JU', hint: 'J + U' },
    { name: 'JE', hint: 'J + E' },
    { name: 'JI', hint: 'J + I' },
    { name: 'KO', hint: 'K + O' },
    { name: 'KU', hint: 'K + U' },
    { name: 'KE', hint: 'K + E' },
    { name: 'KI', hint: 'K + I' },
    { name: 'VO', hint: 'V + O' },
    { name: 'VU', hint: 'V + U' },
    { name: 'VE', hint: 'V + E' },
    { name: 'VI', hint: 'V + I' },
  ];

  // 100 ms at 16 kHz — shorter holds are treated as an accidental tap and
  // re-prompt the same syllable rather than producing a garbage centroid.
  const MIN_SAMPLE_LEN = 1600;

  let {
    stq8Service,
    isCalibrated,
    onCalibrated,
  }: {
    stq8Service: Stq8ServiceLike;
    isCalibrated: boolean;
    onCalibrated: () => void;
  } = $props();

  type Phase = 'intro' | 'requesting' | 'recording' | 'finalizing' | 'done' | 'error';
  let phase = $state<Phase>(isCalibrated ? 'done' : 'intro');

  // Initial $state capture is a snapshot — if the parent's isCalibrated flips
  // true after we mount (async WASM init, then profile import, while the user
  // happened to be sitting on the Calibrate tab), we also need to advance the
  // phase to 'done'. Only advance from 'intro' so we don't clobber an
  // in-progress recording/finalizing flow or an error surface the user should
  // still see.
  $effect(() => {
    if (isCalibrated && phase === 'intro') {
      phase = 'done';
    }
  });
  let currentIndex = $state(0);
  let isHolding = $state(false);
  let errorMsg = $state('');
  let lastSampleWasShort = $state(false);
  // True after a successful saveProfile() in this session's flow. False when
  // localStorage wasn't available or setItem rejected (quota / SecurityError
  // in restricted contexts). Gates the "Your voice profile is saved" copy in
  // the done phase so we don't lie to the user about persistence.
  let profilePersisted = $state(true);

  let audioCapture: AudioCapture | null = null;
  let recordingBuffer: Float32Array[] = [];

  function onFrame(pcm: Float32Array): void {
    if (isHolding) recordingBuffer.push(pcm);
  }

  async function startFlow() {
    if (!stq8Service.isReady()) {
      errorMsg = 'Spellbook engine not loaded yet. Run scripts/build-wasm.sh in the sibling harmony-stq8 clone and reload.';
      phase = 'error';
      return;
    }
    phase = 'requesting';
    errorMsg = '';
    try {
      audioCapture = new AudioCapture();
      await audioCapture.start(onFrame);
      currentIndex = 0;
      lastSampleWasShort = false;
      phase = 'recording';
    } catch (err) {
      errorMsg = err instanceof Error ? err.message : String(err);
      phase = 'error';
      audioCapture = null;
    }
  }

  function handleRecordStart() {
    if (phase !== 'recording' || isHolding) return;
    recordingBuffer = [];
    lastSampleWasShort = false;
    isHolding = true;
  }

  async function handleRecordStop() {
    if (!isHolding) return;
    isHolding = false;
    const totalLen = recordingBuffer.reduce((s, f) => s + f.length, 0);
    if (totalLen < MIN_SAMPLE_LEN) {
      // User tapped instead of holding — keep them on the same syllable.
      lastSampleWasShort = true;
      recordingBuffer = [];
      return;
    }
    const pcm = new Float32Array(totalLen);
    let off = 0;
    for (const f of recordingBuffer) {
      pcm.set(f, off);
      off += f.length;
    }
    recordingBuffer = [];
    try {
      stq8Service.addCalibrationSample(currentIndex, pcm);
    } catch (err) {
      errorMsg = err instanceof Error ? err.message : String(err);
      phase = 'error';
      await stopCapture();
      return;
    }
    currentIndex += 1;
    if (currentIndex >= SYLLABLES.length) {
      await finishCalibration();
    }
  }

  async function finishCalibration() {
    phase = 'finalizing';
    try {
      stq8Service.finalizeCalibration();
      stq8Service.setCreatedEpochSecs(BigInt(Math.floor(Date.now() / 1000)));
      const profileJson = stq8Service.exportProfile();
      profilePersisted = profileStorage.saveProfile(profileJson);
    } catch (err) {
      errorMsg = err instanceof Error ? err.message : String(err);
      phase = 'error';
      await stopCapture();
      return;
    }
    await stopCapture();
    phase = 'done';
    onCalibrated();
  }

  async function stopCapture() {
    if (audioCapture) {
      // AudioCapture.stop() is already non-throwing at the source, but belt-
      // and-suspenders: if anything here ever throws in the future, we don't
      // want to strand the UI in 'finalizing' or bubble an unhandled rejection
      // up through unmount cleanup.
      try {
        await audioCapture.stop();
      } catch (err) {
        console.warn('[harmony-client] stq8 calibration: audio stop failed:', err);
      }
      audioCapture = null;
    }
    isHolding = false;
    recordingBuffer = [];
  }

  function handleRecalibrate() {
    // Deliberately DON'T clear the persisted profile here. If the user denies
    // mic permission or bails mid-flow, we want the previous working profile
    // still in localStorage so the next boot reloads it. saveProfile() in
    // finishCalibration() overwrites atomically when the new flow succeeds.
    phase = 'intro';
    currentIndex = 0;
    errorMsg = '';
    lastSampleWasShort = false;
    profilePersisted = true;
  }

  // On unmount — stop capture so the mic indicator clears and browser
  // resources get released. AudioCapture.stop handles the already-stopped
  // case internally.
  $effect(() => () => { void stopCapture(); });

  let progressPct = $derived(Math.round((currentIndex / SYLLABLES.length) * 100));
  let currentSyllable = $derived(SYLLABLES[currentIndex] ?? SYLLABLES[0]);
</script>

<div class="calibration-view" aria-live="polite">
  {#if phase === 'intro'}
    <header class="intro-header">
      <h2>Calibrate your voice</h2>
      <p class="lead">
        The Spellbook classifier needs a short sample of each of the 16 Q8 syllables in your own voice
        before it can recognize them during practice. You'll hold a button and say each syllable clearly
        while holding — release to move on.
      </p>
      <p class="muted">One-time setup, ~30 seconds. Profile is saved locally and reloads automatically.</p>
    </header>
    <button type="button" class="primary" onclick={startFlow}>Start Calibration</button>
  {:else if phase === 'requesting'}
    <p class="status">Requesting microphone access…</p>
  {:else if phase === 'recording'}
    <div class="recording-ui">
      <div class="progress-bar" role="progressbar" aria-valuemin="0" aria-valuemax={SYLLABLES.length} aria-valuenow={currentIndex}>
        <div class="progress-fill" style:width="{progressPct}%"></div>
      </div>
      <p class="counter">{currentIndex + 1} of {SYLLABLES.length}</p>
      <div class="syllable-display" aria-label="Current syllable">
        <span class="syllable-name">{currentSyllable.name}</span>
        <span class="syllable-hint">{currentSyllable.hint}</span>
      </div>
      <p class="instruction">
        {#if isHolding}
          Listening… keep holding and speak.
        {:else if lastSampleWasShort}
          Too short — hold the button while you speak, then release.
        {:else}
          Hold the record button and clearly say <strong>{currentSyllable.name}</strong>.
        {/if}
      </p>
      <button
        type="button"
        class="record-button"
        class:active={isHolding}
        aria-label="Hold to record {currentSyllable.name}"
        aria-pressed={isHolding}
        onmousedown={handleRecordStart}
        onmouseup={handleRecordStop}
        onmouseleave={handleRecordStop}
        ontouchstart={(e) => { e.preventDefault(); handleRecordStart(); }}
        ontouchend={(e) => { e.preventDefault(); void handleRecordStop(); }}
        ontouchcancel={() => { void handleRecordStop(); }}
        onkeydown={(e) => {
          if ((e.key === ' ' || e.key === 'Enter') && !e.repeat) {
            e.preventDefault();
            handleRecordStart();
          }
        }}
        onkeyup={(e) => {
          if (e.key === ' ' || e.key === 'Enter') {
            e.preventDefault();
            void handleRecordStop();
          }
        }}
        onblur={() => { void handleRecordStop(); }}
      >
        <span aria-hidden="true">🎤</span>
      </button>
    </div>
  {:else if phase === 'finalizing'}
    <p class="status">Finalizing profile…</p>
  {:else if phase === 'done'}
    <div class="done-ui">
      <h2>Calibrated ✓</h2>
      {#if profilePersisted}
        <p>Your voice profile is saved. Head to the Practice tab to try it out.</p>
      {:else}
        <p class="persistence-warning" role="alert">
          Calibration works for this session, but your profile couldn't be saved — you'll need to re-calibrate on the next reload.
          Check the console for details (common causes: storage disabled, quota exceeded, private-browsing context).
        </p>
      {/if}
      <button type="button" class="secondary" onclick={handleRecalibrate}>Recalibrate</button>
    </div>
  {:else if phase === 'error'}
    <div class="error-ui" role="alert">
      <h2>Calibration failed</h2>
      <p class="error-msg">{errorMsg || 'Unknown error.'}</p>
      <button type="button" class="primary" onclick={startFlow}>Try Again</button>
    </div>
  {/if}
</div>

<style>
  .calibration-view {
    padding: 24px;
    max-width: 560px;
    margin: 0 auto;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 16px;
  }

  h2 {
    margin: 0 0 8px;
    color: var(--text-primary, #f2f3f5);
  }

  .lead {
    color: var(--text-secondary, #b5bac1);
    line-height: 1.5;
  }

  .muted {
    color: var(--text-muted, #949ba4);
    font-size: 0.8125rem;
    margin-top: 4px;
  }

  .intro-header {
    text-align: center;
  }

  .status {
    color: var(--text-muted, #949ba4);
    font-size: 0.9375rem;
  }

  .progress-bar {
    width: 100%;
    height: 6px;
    background: var(--bg-tertiary, #313338);
    border-radius: 3px;
    overflow: hidden;
  }

  .progress-fill {
    height: 100%;
    background: var(--accent, #5865f2);
    transition: width 0.2s ease;
  }

  .counter {
    color: var(--text-muted, #949ba4);
    font-size: 0.8125rem;
    margin: 0;
  }

  .syllable-display {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
    padding: 24px;
  }

  .syllable-name {
    font-size: 4rem;
    font-weight: 600;
    color: var(--text-primary, #f2f3f5);
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }

  .syllable-hint {
    color: var(--text-muted, #949ba4);
    font-size: 0.875rem;
  }

  .instruction {
    color: var(--text-secondary, #b5bac1);
    text-align: center;
    margin: 0;
    min-height: 1.5em;
  }

  .recording-ui {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
    width: 100%;
  }

  .record-button {
    width: 72px;
    height: 72px;
    border-radius: 50%;
    border: 3px solid var(--accent, #5865f2);
    background: transparent;
    color: var(--accent, #5865f2);
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: all 0.15s ease;
    font-size: 1.5rem;
  }

  .record-button.active {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
    box-shadow: 0 0 20px rgba(88, 101, 242, 0.4);
  }

  .done-ui {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
    text-align: center;
  }

  .error-ui {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
    text-align: center;
  }

  .error-msg {
    color: var(--text-danger, #ed4245);
    max-width: 480px;
  }

  .persistence-warning {
    color: var(--text-warning, #f0b232);
    max-width: 480px;
    text-align: center;
  }

  button.primary, button.secondary {
    padding: 10px 20px;
    border-radius: 4px;
    font-size: 0.9375rem;
    cursor: pointer;
    border: none;
    font-weight: 500;
  }

  button.primary {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
  }

  button.secondary {
    background: var(--bg-tertiary, #313338);
    color: var(--text-secondary, #b5bac1);
    border: 1px solid var(--border, #3f4147);
  }
</style>
