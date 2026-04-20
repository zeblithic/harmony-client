<script lang="ts">
  import { untrack } from 'svelte';
  import type {
    FlashcardLevel,
    Challenge,
    RowState,
    SessionStats,
    ExpressMode,
  } from '../flashcard-types';
  import { initialSessionStats } from '../flashcard-types';
  import { evaluateBytes } from '../express-lane';
  import type { Stq8ServiceLike } from '../stq8-service';
  import { AudioCapture } from '../voice/audio-capture';
  import FlashcardGrid from './FlashcardGrid.svelte';
  import HintBar from './HintBar.svelte';
  import PttButton from './PttButton.svelte';

  let {
    level,
    expressMode,
    stq8Service,
    initialStats = initialSessionStats(),
    onStatsUpdate,
  }: {
    level: FlashcardLevel;
    expressMode: ExpressMode;
    stq8Service: Stq8ServiceLike;
    initialStats?: SessionStats;
    onStatsUpdate: (stats: SessionStats) => void;
  } = $props();

  let challenge = $state<Challenge | null>(null);
  let activeRowIndex = $state(0);
  let rowStates = $state<RowState[]>([]);
  let pttActive = $state(false);
  let showHint = $state(false);
  // initialStats seeds session state once; subsequent updates are driven
  // locally and flushed back through onStatsUpdate.
  let stats = $state(untrack(() => initialStats));
  // Accumulated active PTT time (ms) for the current card, excluding off-air gaps.
  let cardActiveMs = $state(0);
  let pttSegmentStart = $state<number | null>(null);

  // Voice capture — lazy-started on first PTT press. One AudioCapture
  // instance lives for the lifetime of this component; onFrame buffers
  // raw PCM only while `pttActive` is true, and every PTT release flushes
  // the accumulated buffer through `processPcm` to get classifier output.
  let audioCapture: AudioCapture | null = null;
  let pcmBuffer: Float32Array[] = [];
  let captureError = $state('');

  function onPcmFrame(pcm: Float32Array): void {
    if (pttActive) pcmBuffer.push(pcm);
  }

  async function ensureCapture(): Promise<void> {
    if (audioCapture) return;
    const capture = new AudioCapture();
    try {
      await capture.start(onPcmFrame);
      audioCapture = capture;
      captureError = '';
    } catch (err) {
      // Permission denied, no mic, AudioContext construction failure, etc.
      // Leave `audioCapture` null so the next PTT press will retry; surface
      // a short message in the UI so the user knows why nothing's landing.
      captureError = err instanceof Error ? err.message : String(err);
      console.warn('[harmony-client] flashcard PTT: capture start failed:', err);
    }
  }

  // Generate challenge when level changes (or on mount, since previousLevel starts null).
  // previousLevel is set inside newChallenge() AFTER the ready check passes, so if the
  // service isn't ready yet the effect will re-fire on the next reactive change.
  let previousLevel: FlashcardLevel | null = null;
  $effect(() => {
    if (level !== previousLevel) {
      newChallenge();
    }
  });

  function newChallenge() {
    if (!stq8Service.isReady()) return;
    previousLevel = level;
    challenge = stq8Service.generateChallenge(level);
    activeRowIndex = 0;
    rowStates = [];
    // If PTT is already held (auto-advance), start timing immediately
    cardActiveMs = 0;
    pttSegmentStart = pttActive ? Date.now() : null;
  }

  async function handlePttStart() {
    pcmBuffer = [];
    pttActive = true;
    pttSegmentStart = Date.now();
    // Kick off capture on first press. Subsequent presses reuse the
    // running instance; onPcmFrame's pttActive gate handles framing.
    // If the user releases before start() resolves, the buffer stays
    // empty and handlePttStop falls through to the cancel path.
    await ensureCapture();
  }

  function handlePttStop() {
    pttActive = false;
    // Bank active time from this PTT segment
    if (pttSegmentStart !== null) {
      cardActiveMs += Date.now() - pttSegmentStart;
      pttSegmentStart = null;
    }

    const nibbles = flushAndClassify();
    if (nibbles && nibbles.length > 0) {
      // Classifier heard something — let the existing row-completion
      // logic handle pass/fail/combo. handleRowComplete already deals
      // with red-flash-and-reset on mismatch and combo bookkeeping on
      // success, so we don't need to duplicate any of that here.
      handleRowComplete(nibbles);
      return;
    }

    // No PCM captured (capture still starting up, or empty hold), or
    // classifier returned no syllables, or processPcm threw. Fall back
    // to the pre-Slice-3 behavior: cancel the in-progress row and break
    // the combo streak. Design spec: "without PTT release or timeout".
    rowStates = rowStates.filter(s => s.rowIndex !== activeRowIndex || s.completed);
    if (stats.combo > 0) {
      stats = { ...stats, combo: 0 };
      onStatsUpdate(stats);
    }
  }

  function flushAndClassify(): number[] | null {
    const buffered = pcmBuffer;
    pcmBuffer = [];
    if (buffered.length === 0) return null;

    let totalLen = 0;
    for (const f of buffered) totalLen += f.length;
    const pcm = new Float32Array(totalLen);
    let off = 0;
    for (const f of buffered) {
      pcm.set(f, off);
      off += f.length;
    }

    try {
      const result = stq8Service.processPcm(pcm);
      return result.syllables.map(s => s.nibble);
    } catch (err) {
      console.warn('[harmony-client] flashcard PTT: processPcm failed:', err);
      return null;
    }
  }

  function handleRowComplete(heardNibbles: number[]) {
    if (!challenge) return;
    const row = challenge.rows[activeRowIndex];
    if (!row) return;

    const { results, hasRed } = evaluateBytes(
      row, heardNibbles, expressMode,
    );

    if (hasRed) {
      // Brief red flash, then reset row
      const failedRowIndex = activeRowIndex;
      rowStates = [
        ...rowStates.filter(s => s.rowIndex !== failedRowIndex),
        { rowIndex: failedRowIndex, byteResults: results, completed: false },
      ];
      setTimeout(() => {
        rowStates = rowStates.filter(s => s.rowIndex !== failedRowIndex || s.completed);
      }, 300);
      stats = { ...stats, combo: 0 };
      onStatsUpdate(stats);
      return;
    }

    // Row passed — bank it
    const newRowState = { rowIndex: activeRowIndex, byteResults: results, completed: true };
    rowStates = [
      ...rowStates.filter(s => s.rowIndex !== activeRowIndex),
      newRowState,
    ];

    // Check if card is complete
    if (activeRowIndex >= challenge.rows.length - 1) {
      handleCardComplete();
    } else {
      activeRowIndex++;
    }
  }

  function handleCardComplete() {
    // Total active PTT time: accumulated segments + current segment (if PTT still held)
    const elapsed = cardActiveMs + (pttSegmentStart !== null ? Date.now() - pttSegmentStart : 0);

    // Sum credited bits from all completed rows (including the last row,
    // which was already pushed to rowStates before this call).
    let totalBits = 0;
    for (const rs of rowStates) {
      if (rs.completed) {
        totalBits += rs.byteResults.filter(r => r === 'green').length * 8
          + rs.byteResults.filter(r => r === 'yellow').length * 4;
      }
    }

    const hasYellow = rowStates.some(s =>
      s.byteResults.some(r => r === 'yellow')
    );

    const newStats: SessionStats = {
      cardsCompleted: stats.cardsCompleted + 1,
      perfectCards: stats.perfectCards + (hasYellow ? 0 : 1),
      expressCards: stats.expressCards + (hasYellow ? 1 : 0),
      bestTimeMs: stats.bestTimeMs === null
        ? elapsed
        : Math.min(stats.bestTimeMs, elapsed),
      totalTimeMs: stats.totalTimeMs + elapsed,
      previousTimeMs: elapsed,
      combo: stats.combo + 1,
      totalCreditedBits: stats.totalCreditedBits + totalBits,
    };

    stats = newStats;
    onStatsUpdate(newStats);

    // Auto-advance to next card
    newChallenge();
  }

  // Release the mic when this component unmounts (tab switch out of
  // Practice, or navigation away from Spellbook). AudioCapture.stop()
  // is safe to call on a never-started instance and internally handles
  // already-stopped state.
  $effect(() => () => {
    const c = audioCapture;
    audioCapture = null;
    pttActive = false;
    pcmBuffer = [];
    if (c) void c.stop();
  });

  // Flat text for active row hint
  let hintText = $derived.by(() => {
    if (!challenge || !challenge.rows[activeRowIndex]) return '';
    const FLAT_CONSONANTS = ["'", 'J', 'K', 'V'];
    const FLAT_VOWELS = ['O', 'U', 'E', 'I'];
    const row = challenge.rows[activeRowIndex];
    return row
      .map((byte) => {
        const high = (byte >> 4) & 0x0f;
        const low = byte & 0x0f;
        const s1 = FLAT_CONSONANTS[(high >> 2) & 3] + FLAT_VOWELS[high & 3];
        const s2 = FLAT_CONSONANTS[(low >> 2) & 3] + FLAT_VOWELS[low & 3];
        return s1 + s2;
      })
      .join(' ');
  });
</script>

{#if !stq8Service.isReady()}
  <div class="flashcard-loading">
    <p>Loading Q8 engine...</p>
  </div>
{:else if challenge}
  <div class="flashcard-view">
    <div class="grid-container">
      <FlashcardGrid
        rows={challenge.rows}
        {activeRowIndex}
        {rowStates}
        {level}
      />
    </div>

    <div class="controls">
      <button
        type="button"
        class="hint-toggle"
        class:active={showHint}
        aria-label="Toggle hint"
        onclick={() => {
          showHint = !showHint;
        }}
      >
        Hint
      </button>
    </div>

    <HintBar flatText={hintText} visible={showHint} />

    <div class="ptt-container">
      <PttButton
        active={pttActive}
        disabled={!stq8Service.isCalibrated()}
        onPttStart={handlePttStart}
        onPttStop={handlePttStop}
      />
      {#if !stq8Service.isCalibrated()}
        <p class="ptt-hint">Calibrate your voice on the Calibrate tab to enable Practice.</p>
      {:else if captureError}
        <p class="ptt-hint error" role="alert">Microphone: {captureError}</p>
      {/if}
    </div>
  </div>
{/if}

<style>
  .flashcard-view {
    display: flex;
    flex-direction: column;
    height: 100%;
    padding: 24px;
  }

  .flashcard-loading {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--text-muted, #949ba4);
  }

  .grid-container {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .controls {
    display: flex;
    justify-content: center;
    gap: 8px;
    padding: 8px 0;
  }

  .hint-toggle {
    padding: 4px 12px;
    border: 1px solid var(--border, #3f4147);
    border-radius: 4px;
    background: var(--bg-tertiary, #313338);
    color: var(--text-secondary, #b5bac1);
    cursor: pointer;
    font-size: 0.8125rem;
  }

  .hint-toggle.active {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
    border-color: var(--accent, #5865f2);
  }

  .ptt-container {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    padding: 16px 0;
  }

  .ptt-hint {
    margin: 0;
    color: var(--text-muted, #949ba4);
    font-size: 0.8125rem;
    text-align: center;
    max-width: 420px;
  }

  .ptt-hint.error {
    color: var(--text-warning, #f0b232);
  }
</style>
