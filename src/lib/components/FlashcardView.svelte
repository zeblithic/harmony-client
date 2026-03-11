<script lang="ts">
  import type {
    FlashcardLevel,
    Challenge,
    RowState,
    SessionStats,
  } from '../flashcard-types';
  import { initialSessionStats } from '../flashcard-types';
  import { evaluateBytes } from '../express-lane';
  import FlashcardGrid from './FlashcardGrid.svelte';
  import HintBar from './HintBar.svelte';
  import PttButton from './PttButton.svelte';

  let {
    level,
    expressLane,
    stq8Service,
    initialStats = initialSessionStats(),
    onStatsUpdate,
  }: {
    level: FlashcardLevel;
    expressLane: boolean;
    stq8Service: {
      isReady(): boolean;
      getLevelInfo(l: FlashcardLevel): {
        total_bytes: number;
        bytes_per_row: number;
        num_rows: number;
        total_bits: number;
      };
      generateChallenge(l: FlashcardLevel): Challenge;
    };
    initialStats?: SessionStats;
    onStatsUpdate: (stats: SessionStats) => void;
  } = $props();

  let challenge = $state<Challenge | null>(null);
  let activeRowIndex = $state(0);
  let rowStates = $state<RowState[]>([]);
  let pttActive = $state(false);
  let showHint = $state(false);
  let stats = $state(initialStats);
  // Accumulated active PTT time (ms) for the current card, excluding off-air gaps.
  let cardActiveMs = $state(0);
  let pttSegmentStart = $state<number | null>(null);

  // Generate challenge when level changes (or on mount, since previousLevel starts null)
  let previousLevel: FlashcardLevel | null = null;
  $effect(() => {
    if (level !== previousLevel) {
      previousLevel = level;
      newChallenge();
    }
  });

  function newChallenge() {
    if (!stq8Service.isReady()) return;
    challenge = stq8Service.generateChallenge(level);
    activeRowIndex = 0;
    rowStates = [];
    // If PTT is already held (auto-advance), start timing immediately
    cardActiveMs = 0;
    pttSegmentStart = pttActive ? Date.now() : null;
  }

  function handlePttStart() {
    pttActive = true;
    pttSegmentStart = Date.now();
  }

  function handlePttStop() {
    pttActive = false;
    // Bank active time from this PTT segment
    if (pttSegmentStart !== null) {
      cardActiveMs += Date.now() - pttSegmentStart;
      pttSegmentStart = null;
    }
    // Release PTT = cancel current row (banked rows kept)
    rowStates = rowStates.filter(s => s.rowIndex !== activeRowIndex || s.completed);
    // PTT release breaks the combo streak (design spec: "without PTT release or timeout")
    if (stats.combo > 0) {
      stats = { ...stats, combo: 0 };
      onStatsUpdate(stats);
    }
  }

  function handleRowComplete(heardNibbles: number[]) {
    if (!challenge) return;
    const row = challenge.rows[activeRowIndex];
    if (!row) return;

    const { results, hasRed } = evaluateBytes(
      row, heardNibbles, expressLane,
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
        onPttStart={handlePttStart}
        onPttStop={handlePttStop}
      />
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
    justify-content: center;
    padding: 16px 0;
  }
</style>
