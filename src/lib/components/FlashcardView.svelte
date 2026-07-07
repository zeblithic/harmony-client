<script lang="ts">
  import { untrack } from 'svelte';
  import type {
    FlashcardLevel,
    Challenge,
    RowState,
    SessionStats,
    ExpressMode,
    ByteResult,
  } from '../flashcard-types';
  import { initialSessionStats } from '../flashcard-types';
  import { evaluateBytes, failingNibbleInRedByte } from '../express-lane';
  import { formatFlatBytes, findFirstNibbleDiff } from '../q8-flat';
  import type { Stq8ServiceLike } from '../stq8-service';
  import { AudioCapture } from '../voice/audio-capture';
  import FlashcardGrid from './FlashcardGrid.svelte';
  import HintBar from './HintBar.svelte';
  import MismatchDisplay from './MismatchDisplay.svelte';
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

  // Tick cadence for mid-hold classification. 300 ms is short enough to
  // feel live at the ~250–400 ms speaking pace of a Q8 syllable and long
  // enough to amortize the processPcm call cost over several AudioWorklet
  // frames.
  const TICK_INTERVAL_MS = 300;
  // Momentum timeout (flashcard-design.md §PTT Rules): if 2 s pass without
  // a successfully classified syllable advancing position, the row resets
  // while PTT stays held.
  const MOMENTUM_TIMEOUT_MS = 2000;

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
  // raw PCM only while `pttActive` is true. A periodic tick drains that
  // buffer through processPcm so the classifier runs continuously across
  // the hold rather than only on release.
  let audioCapture: AudioCapture | null = null;
  // Frames captured since the last processTick drain. processTick swaps
  // this out atomically on each tick (push and swap both run on the main
  // thread, so no lock needed).
  let pcmBuffer: Float32Array[] = [];
  let captureError = $state('');

  // Mid-hold classification state. heardNibbles accumulates classified
  // syllables for the current row attempt; lastEvaluatedByteCount pins
  // the boundary already checked against the expected row so we don't
  // re-evaluate every tick.
  let heardNibbles = $state<number[]>([]);
  let lastEvaluatedByteCount = 0;
  // Timestamp of the last syllable classified (any new syllable counts
  // as position advancement for the momentum timer). null when no hold
  // is active.
  let lastAdvanceAt: number | null = null;
  let tickTimer: ReturnType<typeof setInterval> | null = null;
  // Pending handle for the 300 ms red-flash cleanup scheduled in
  // handleRowMismatch. Held so a subsequent mismatch on the same row
  // can cancel the previous cleanup before scheduling a fresh one —
  // otherwise the old setTimeout's closure would fire on the new
  // red-flash rowState and wipe it prematurely.
  let redFlashClear: ReturnType<typeof setTimeout> | null = null;
  // Mismatch feedback for the MismatchDisplay component. Set whenever a
  // byte-boundary check finds a red, cleared on successful row advance
  // or on next PTT press.
  let mismatch = $state<{
    expectedBytes: number[];
    heardNibbles: number[];
    firstDiffNibbleIdx: number;
  } | null>(null);

  // Serialize concurrent ensureCapture() calls. Without this, rapid
  // press/release cycles before the first getUserMedia resolves can
  // start multiple AudioCapture instances — the second overwrites
  // audioCapture and orphans the first with an open mic that cleanup
  // can't reach. captureStart pins the in-flight promise so subsequent
  // callers await the same result instead of kicking off another start.
  let captureStart: Promise<void> | null = null;
  // destroyed lets a pending start know the component has unmounted. If
  // start() resolves after cleanup ran, we stop the capture immediately
  // rather than assigning it to the now-unmounted component — otherwise
  // the mic stays live forever because cleanup saw audioCapture=null.
  let destroyed = false;

  function onPcmFrame(pcm: Float32Array): void {
    if (pttActive) pcmBuffer.push(pcm);
  }

  function ensureCapture(): Promise<void> {
    if (audioCapture) return Promise.resolve();
    if (captureStart) return captureStart;
    const capture = new AudioCapture();
    captureStart = (async () => {
      try {
        await capture.start(onPcmFrame);
        if (destroyed) {
          // Unmounted while start was pending — release the mic now.
          // cleanup couldn't reach it because audioCapture was still null.
          await capture.stop();
          return;
        }
        audioCapture = capture;
        captureError = '';
      } catch (err) {
        // Permission denied, no mic, AudioContext construction failure, etc.
        // Leave `audioCapture` null so the next PTT press will retry; surface
        // a short message in the UI so the user knows why nothing's landing.
        if (destroyed) return;
        captureError = err instanceof Error ? err.message : String(err);
        console.warn('[harmony-client] flashcard PTT: capture start failed:', err);
      } finally {
        captureStart = null;
      }
    })();
    return captureStart;
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
    resetAttempt();
    mismatch = null;
    pttActive = true;
    await ensureCapture();
    // Re-check state after the await: PttButton fires onPttStart synchronously
    // without awaiting, so handlePttStop (or unmount) can run between the
    // sync prelude above and this resumption. Starting a setInterval after
    // release would leave an orphan tick that eventually fires
    // handleRowTimeout and resets combo. Likewise, starting a tick on a
    // destroyed component leaks the timer.
    //
    // `audioCapture` is also checked because ensureCapture swallows start
    // failures (permission denied, no device, AudioContext error) and
    // leaves the field null while the returned promise still resolves
    // normally. Arming the tick without a live mic would treat the
    // resulting silence as a momentum timeout after 2 s and zero combo
    // on a user who never actually had capture running.
    if (destroyed || !pttActive || !audioCapture) return;
    // Seed timers AFTER capture is actually live. If the first-use
    // permission prompt or AudioContext construction took >2 s, an
    // earlier-seeded lastAdvanceAt would trip the first tick's timeout
    // check immediately — a phantom timeout for a mic the user hadn't
    // even granted yet. pttSegmentStart is moved here too so the "active
    // PTT time" metric only counts time on a live mic.
    pttSegmentStart = Date.now();
    lastAdvanceAt = Date.now();
    startTick();
  }

  function handlePttStop() {
    pttActive = false;
    stopTick();
    // Defensive: even if a late-resolving ensureCapture still manages to
    // call startTick() after this (shouldn't happen post-guard, but
    // belt-and-suspenders), a null lastAdvanceAt makes checkTimeout a no-op.
    lastAdvanceAt = null;
    // Bank active time from this PTT segment.
    if (pttSegmentStart !== null) {
      cardActiveMs += Date.now() - pttSegmentStart;
      pttSegmentStart = null;
    }
    // Snapshot pre-flush state so the final flush can't confuse a successful
    // card completion with an abandoned attempt. If the flush completes a
    // card, heardNibbles retains the carry nibbles for the new card's row 0
    // — interpreting that carry as "the user abandoned a row mid-attempt"
    // would undo the combo increment handleCardComplete just published.
    //
    // Mismatch state (mismatch !== null) is also carved out: design says
    // "No penalty on wrong answers", which must apply even when release
    // lands inside the 300 ms red-flash window or the final flush itself
    // triggered a mismatch. Treating the red-flash rowState or the in-
    // flush mismatch path as an abandonable partial attempt would break
    // combo on exactly the scenarios the no-penalty rule exists to
    // cover. Only heardNibbles — classified syllables with no mismatch
    // in play — counts as a partial attempt for combo-reset purposes.
    const preFlushActiveRow = activeRowIndex;
    const preFlushHadAttempt = heardNibbles.length > 0;
    const mismatchBeforeFlush = mismatch !== null;
    const preFlushCardsCompleted = stats.cardsCompleted;

    // Final flush: any frames captured between the last tick and release
    // still flow through processPcm. A row completion or mismatch discovered
    // in the flush uses the same handlers as a mid-hold tick.
    processTick({ finalFlush: true });

    const cardCompletedDuringFlush = stats.cardsCompleted > preFlushCardsCompleted;
    const mismatchAfterFlush = mismatch !== null;
    // Attempts that first appear in the flush itself also count: a short
    // hold that never got a mid-hold tick can still have its initial
    // syllables classified by this one flush call. Without the post-flush
    // check, those attempts slip past the combo-reset and get silently
    // discarded by resetAttempt() below.
    const postFlushHadAttempt = heardNibbles.length > 0;
    // Release-cancels-row per spec: "Release PTT = cancel current row." Only
    // fires when the user was genuinely mid-attempt (pre- or during-flush
    // classified syllables), no card completion rescued by the flush, and
    // no mismatch in play on either side of the flush. Filter by
    // preFlushActiveRow because the flush may have advanced activeRowIndex.
    if (
      (preFlushHadAttempt || postFlushHadAttempt) &&
      !cardCompletedDuringFlush &&
      !mismatchBeforeFlush &&
      !mismatchAfterFlush
    ) {
      rowStates = rowStates.filter(
        (s) => s.rowIndex !== preFlushActiveRow || s.completed,
      );
      if (stats.combo > 0) {
        stats = { ...stats, combo: 0 };
        onStatsUpdate(stats);
      }
    }
    resetAttempt();
  }

  function resetAttempt() {
    pcmBuffer = [];
    heardNibbles = [];
    lastEvaluatedByteCount = 0;
  }

  function startTick() {
    stopTick();
    tickTimer = setInterval(() => processTick({ finalFlush: false }), TICK_INTERVAL_MS);
  }

  function stopTick() {
    if (tickTimer !== null) {
      clearInterval(tickTimer);
      tickTimer = null;
    }
  }

  function processTick(opts: { finalFlush: boolean }): void {
    const frames = pcmBuffer;
    pcmBuffer = [];

    if (frames.length === 0) {
      if (!opts.finalFlush) checkTimeout();
      return;
    }

    // Concat new frames into one Float32Array for a single processPcm call.
    let totalLen = 0;
    for (const f of frames) totalLen += f.length;
    const pcm = new Float32Array(totalLen);
    let off = 0;
    for (const f of frames) {
      pcm.set(f, off);
      off += f.length;
    }

    let newNibbles: number[];
    try {
      const result = stq8Service.processPcm(pcm);
      newNibbles = result.syllables.map((s) => s.nibble);
    } catch (err) {
      console.warn('[harmony-client] flashcard PTT: processPcm failed:', err);
      if (!opts.finalFlush) checkTimeout();
      return;
    }

    if (newNibbles.length === 0) {
      if (!opts.finalFlush) checkTimeout();
      return;
    }

    // Any new syllable counts as advancement for the momentum timer —
    // partial-byte progress (an odd-length heardNibbles) is still a syllable
    // classified, not a no-op.
    heardNibbles = [...heardNibbles, ...newNibbles];
    lastAdvanceAt = Date.now();
    evaluateProgress();
    if (!opts.finalFlush) checkTimeout();
  }

  function evaluateProgress(): void {
    // Loop consumes heardNibbles across row boundaries when a single
    // utterance covers multiple rows (common on Novice/Apprentice). Each
    // iteration either detects a mismatch (returns), consumes a full row
    // and continues against the next row, or hits a partial-row boundary
    // and returns to await more classifier output.
    while (challenge) {
      const row = challenge.rows[activeRowIndex];
      if (!row) return;
      const completeByteCount = Math.floor(heardNibbles.length / 2);
      if (completeByteCount <= lastEvaluatedByteCount) return;

      // Byte-boundary evaluation: only check bytes we haven't already
      // accepted. evaluateBytes over a partial row returns red for any
      // byte that fails strict or express matching — that's our mismatch
      // trigger.
      const partialRow = row.slice(0, completeByteCount);
      const partialHeard = heardNibbles.slice(0, completeByteCount * 2);
      const { results, hasRed } = evaluateBytes(
        partialRow,
        partialHeard,
        expressMode,
      );
      if (hasRed) {
        handleRowMismatch(row, [...heardNibbles], results);
        return;
      }
      lastEvaluatedByteCount = completeByteCount;

      const expectedNibbleCount = row.length * 2;
      if (heardNibbles.length < expectedNibbleCount) return;

      // Full row classified — bank it. Any heard nibbles past the row
      // length carry over to the next row so a fast multi-row utterance
      // isn't truncated at boundaries.
      completeCurrentRow(row, results);
    }
  }

  function completeCurrentRow(row: number[], results: ByteResult[]) {
    if (!challenge) return;
    const expectedNibbleCount = row.length * 2;
    const carry = heardNibbles.slice(expectedNibbleCount);

    const newRowState: RowState = {
      rowIndex: activeRowIndex,
      byteResults: results,
      completed: true,
    };
    rowStates = [
      ...rowStates.filter((s) => s.rowIndex !== activeRowIndex),
      newRowState,
    ];

    heardNibbles = carry;
    lastEvaluatedByteCount = 0;
    lastAdvanceAt = Date.now();
    mismatch = null;

    if (activeRowIndex >= challenge.rows.length - 1) {
      handleCardComplete();
      // newChallenge() has reset activeRowIndex=0 and rowStates=[]. The
      // evaluateProgress loop continues with `carry` against the new card.
    } else {
      activeRowIndex++;
    }
  }

  function handleRowMismatch(
    row: number[],
    heardSnapshot: number[],
    results: ByteResult[],
  ) {
    // Brief red flash on the grid (same visual as pre-Slice-4). Any
    // pending cleanup from a previous mismatch on any row must be
    // canceled first — otherwise its closure would fire 300 ms after
    // THAT mismatch and strip the new red-flash rowState on the same
    // row (mid-hold ticks are 300 ms apart, so back-to-back mismatches
    // land exactly at the stale timeout's deadline).
    if (redFlashClear !== null) clearTimeout(redFlashClear);
    const failedRowIndex = activeRowIndex;
    rowStates = [
      ...rowStates.filter((s) => s.rowIndex !== failedRowIndex),
      { rowIndex: failedRowIndex, byteResults: results, completed: false },
    ];
    redFlashClear = setTimeout(() => {
      rowStates = rowStates.filter(
        (s) => s.rowIndex !== failedRowIndex || s.completed,
      );
      redFlashClear = null;
    }, 300);

    // Caret points at the first truly-failing nibble in the first red byte.
    // In strict mode this coincides with the first strict diff, but in
    // express mode a leading byte may strictly differ yet still be
    // accepted as yellow — the caret must point at what actually failed,
    // not at an express-accepted divergence. handleRowMismatch is only
    // called when hasRed=true, so findIndex should return >= 0; the
    // findFirstNibbleDiff fallback exists only as a safety net.
    const firstRedByteIdx = results.findIndex((r) => r === 'red');
    const firstDiffNibbleIdx =
      firstRedByteIdx >= 0
        ? firstRedByteIdx * 2 +
          failingNibbleInRedByte(
            row[firstRedByteIdx],
            heardSnapshot[firstRedByteIdx * 2],
            heardSnapshot[firstRedByteIdx * 2 + 1],
            expressMode,
          )
        : findFirstNibbleDiff(row, heardSnapshot);

    mismatch = {
      expectedBytes: row,
      heardNibbles: heardSnapshot,
      firstDiffNibbleIdx,
    };

    // Reset for retry. No combo penalty — design: "No penalty on wrong answers."
    heardNibbles = [];
    lastEvaluatedByteCount = 0;
    lastAdvanceAt = Date.now();
  }

  function handleRowTimeout() {
    const failedRowIndex = activeRowIndex;
    rowStates = rowStates.filter(
      (s) => s.rowIndex !== failedRowIndex || s.completed,
    );
    heardNibbles = [];
    lastEvaluatedByteCount = 0;
    lastAdvanceAt = Date.now();
    mismatch = null;

    // Timeout breaks combo per spec: "consecutive cards passed without
    // PTT release or timeout".
    if (stats.combo > 0) {
      stats = { ...stats, combo: 0 };
      onStatsUpdate(stats);
    }
  }

  function checkTimeout(): void {
    if (lastAdvanceAt === null) return;
    if (Date.now() - lastAdvanceAt >= MOMENTUM_TIMEOUT_MS) {
      handleRowTimeout();
    }
  }

  function handleCardComplete() {
    // Total active PTT time: accumulated segments + current segment (if PTT still held)
    const elapsed =
      cardActiveMs +
      (pttSegmentStart !== null ? Date.now() - pttSegmentStart : 0);

    // Sum credited bits from all completed rows (including the last row,
    // which was already pushed to rowStates before this call).
    let totalBits = 0;
    for (const rs of rowStates) {
      if (rs.completed) {
        totalBits +=
          rs.byteResults.filter((r) => r === 'green').length * 8 +
          rs.byteResults.filter((r) => r === 'yellow').length * 4;
      }
    }

    const hasYellow = rowStates.some((s) =>
      s.byteResults.some((r) => r === 'yellow'),
    );

    const newStats: SessionStats = {
      cardsCompleted: stats.cardsCompleted + 1,
      perfectCards: stats.perfectCards + (hasYellow ? 0 : 1),
      expressCards: stats.expressCards + (hasYellow ? 1 : 0),
      bestTimeMs:
        stats.bestTimeMs === null
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
  // Practice, or navigation away from Spellbook). If a capture start is
  // still in flight when we unmount, the `destroyed` flag tells the
  // pending IIFE inside ensureCapture to stop the capture itself once
  // start() resolves — cleanup here can only reach already-assigned
  // captures, so the destroyed-flag path is the only way to guarantee
  // no orphaned mic.
  $effect(() => () => {
    destroyed = true;
    stopTick();
    if (redFlashClear !== null) {
      clearTimeout(redFlashClear);
      redFlashClear = null;
    }
    const c = audioCapture;
    audioCapture = null;
    pttActive = false;
    pcmBuffer = [];
    if (c) void c.stop();
  });

  // Flat text for active row hint
  let hintText = $derived.by(() => {
    if (!challenge || !challenge.rows[activeRowIndex]) return '';
    return formatFlatBytes(challenge.rows[activeRowIndex]);
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

    {#if mismatch}
      <div class="mismatch-slot">
        <MismatchDisplay
          expectedBytes={mismatch.expectedBytes}
          heardNibbles={mismatch.heardNibbles}
          firstDiffNibbleIdx={mismatch.firstDiffNibbleIdx}
        />
      </div>
    {/if}

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
    color: var(--text-muted);
  }

  .grid-container {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .mismatch-slot {
    display: flex;
    justify-content: center;
    padding: 4px 0;
  }

  .controls {
    display: flex;
    justify-content: center;
    gap: 8px;
    padding: 8px 0;
  }

  .hint-toggle {
    padding: 4px 12px;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 0.8125rem;
  }

  .hint-toggle.active {
    background: var(--accent);
    color: var(--text-primary);
    border-color: var(--accent);
  }

  .ptt-container {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    padding: 16px 0;
  }

  .ptt-hint {
    margin: 0;
    color: var(--text-muted);
    font-size: 0.8125rem;
    text-align: center;
    max-width: 420px;
  }

  .ptt-hint.error {
    color: var(--text-warning);
  }
</style>
