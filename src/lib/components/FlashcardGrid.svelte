<script lang="ts">
  import type { RowState, ByteResult } from '../flashcard-types';
  import { byteToBoxCell } from '../q8-utils';

  let {
    rows,
    activeRowIndex,
    rowStates,
    level = 0,
  }: {
    rows: number[][];
    activeRowIndex: number;
    rowStates: RowState[];
    level?: number;
  } = $props();

  function getByteResult(rowIdx: number, byteIdx: number): ByteResult {
    const state = rowStates.find(s => s.rowIndex === rowIdx);
    if (!state) return 'pending';
    return state.byteResults[byteIdx] ?? 'pending';
  }

  // Font size scales with level: larger for easier levels
  let fontSize = $derived(
    level <= 1 ? '2rem' : level <= 2 ? '1.5rem' : level <= 3 ? '1.25rem' : '1rem'
  );
</script>

<div class="flashcard-grid" role="grid" aria-label="Q8-BOX challenge grid" style="font-size: {fontSize}">
  {#each rows as row, rowIdx}
    <div
      class="grid-row"
      class:active={rowIdx === activeRowIndex}
      class:completed-perfect={rowStates.some(s => s.rowIndex === rowIdx && s.completed && s.byteResults.every(r => r === 'green'))}
      class:completed-express={rowStates.some(s => s.rowIndex === rowIdx && s.completed && s.byteResults.some(r => r === 'yellow'))}
      data-testid="grid-row"
      role="row"
    >
      {#each row as byte, byteIdx}
        {@const cell = byteToBoxCell(byte)}
        {@const result = getByteResult(rowIdx, byteIdx)}
        <div
          class="byte-cell {result}"
          data-testid="byte-cell"
          role="gridcell"
          aria-label="Byte {byte.toString(16).padStart(2, '0').toUpperCase()}"
        >
          <div class="consonant-row">{cell.topLeft}{cell.topRight}</div>
          <div class="vowel-row">{cell.bottomLeft}{cell.bottomRight}</div>
        </div>
      {/each}
    </div>
  {/each}
</div>

<style>
  .flashcard-grid {
    display: flex;
    flex-direction: column;
    gap: 8px;
    font-family: 'Courier New', Courier, monospace;
    user-select: none;
  }

  .grid-row {
    display: flex;
    gap: 12px;
    justify-content: center;
    padding: 4px 8px;
    border-radius: 6px;
    border: 2px solid transparent;
    transition: border-color 0.15s ease;
  }

  .grid-row.active {
    border-color: var(--accent);
  }

  .grid-row.completed-perfect {
    background: rgba(87, 242, 135, 0.05);
  }

  .grid-row.completed-express {
    background: rgba(254, 231, 92, 0.05);
  }

  .byte-cell {
    display: flex;
    flex-direction: column;
    align-items: center;
    line-height: 1.1;
    letter-spacing: 0;
    padding: 4px 6px;
    border-radius: 4px;
    transition: background 0.15s ease;
  }

  .consonant-row, .vowel-row {
    white-space: pre;
  }

  .byte-cell.green {
    background: rgba(87, 242, 135, 0.2);
    color: #57f287;
  }

  .byte-cell.yellow {
    background: rgba(254, 231, 92, 0.2);
    color: #fee75c;
  }

  .byte-cell.red {
    background: rgba(237, 66, 69, 0.3);
    color: #ed4245;
    animation: flash-red 0.3s ease;
  }

  @keyframes flash-red {
    0% { background: rgba(237, 66, 69, 0.6); }
    100% { background: rgba(237, 66, 69, 0.3); }
  }
</style>
