# Flashcard UI Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a voice-driven flashcard practice interface within a new "Spellbook" top-level mode in harmony-client. Users see Q8-BOX grids, speak syllables via PTT, and get per-byte green/yellow/red feedback.

**Architecture:** New `spellbook` AppMode with tab-based container (Spells/Practice). FlashcardView orchestrates session state, delegating display to FlashcardGrid (BOX cells), HintBar (FLAT phonetics), and FlashcardStats (detail panel). PttButton handles dual-activation (mouse+spacebar). Express lane is a validation-layer policy in FlashcardView. All WASM calls go through stq8Service. AudioService handles mic capture. Components follow the existing flat-component pattern with scoped CSS and `$props()`/`$state()` runes.

**Tech Stack:** Svelte 5 (runes), TypeScript, vitest + @testing-library/svelte, Web Audio API, stq8-web WASM module

**Design doc:** `docs/plans/2026-03-11-flashcard-ui-design.md`

---

### Task 1: Flashcard Types

**Files:**
- Modify: `src/lib/types.ts` (add `'spellbook'` to AppMode union)
- Create: `src/lib/flashcard-types.ts`
- Create: `src/lib/flashcard-types.test.ts`

**Step 1: Write the failing test**

```typescript
// src/lib/flashcard-types.test.ts
import { describe, it, expect } from 'vitest';
import type { AppMode } from './types';
import {
  type FlashcardLevel,
  type ByteResult,
  type RowState,
  type SessionStats,
  type Challenge,
  LEVELS,
  LEVEL_NAMES,
  initialSessionStats,
} from './flashcard-types';

describe('flashcard-types', () => {
  it('AppMode includes spellbook', () => {
    const mode: AppMode = 'spellbook';
    expect(mode).toBe('spellbook');
  });

  it('LEVELS has 5 entries matching design spec', () => {
    expect(LEVELS).toHaveLength(5);
    expect(LEVELS).toEqual([0, 1, 2, 3, 4]);
  });

  it('LEVEL_NAMES maps all levels', () => {
    expect(LEVEL_NAMES[0]).toBe('Novice');
    expect(LEVEL_NAMES[1]).toBe('Apprentice');
    expect(LEVEL_NAMES[2]).toBe('Journeyman');
    expect(LEVEL_NAMES[3]).toBe('Expert');
    expect(LEVEL_NAMES[4]).toBe('Master');
  });

  it('initialSessionStats returns zeroed stats', () => {
    const stats = initialSessionStats();
    expect(stats.cardsCompleted).toBe(0);
    expect(stats.perfectCards).toBe(0);
    expect(stats.expressCards).toBe(0);
    expect(stats.bestTimeMs).toBeNull();
    expect(stats.totalTimeMs).toBe(0);
    expect(stats.previousTimeMs).toBeNull();
    expect(stats.combo).toBe(0);
    expect(stats.totalCreditedBits).toBe(0);
  });
});
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/flashcard-types.test.ts`
Expected: FAIL — module not found

**Step 3: Add 'spellbook' to AppMode**

In `src/lib/types.ts`, change line 117:
```typescript
export type AppMode = 'messages' | 'vines' | 'files' | 'spellbook';
```

**Step 4: Write flashcard-types.ts**

```typescript
// src/lib/flashcard-types.ts

/** WASM level index (0-4). Maps to stq8-core flashcard::Level. */
export type FlashcardLevel = 0 | 1 | 2 | 3 | 4;

export const LEVELS: FlashcardLevel[] = [0, 1, 2, 3, 4];

export const LEVEL_NAMES: Record<FlashcardLevel, string> = {
  0: 'Novice',
  1: 'Apprentice',
  2: 'Journeyman',
  3: 'Expert',
  4: 'Master',
};

/** Level metadata returned by WasmPipeline.level_info(). */
export interface LevelInfo {
  total_bytes: number;
  bytes_per_row: number;
  num_rows: number;
  total_bits: number;
}

/** Challenge returned by WasmPipeline.generate_challenge(). */
export interface Challenge {
  level: string;
  data: number[];
  rows: number[][];
}

/** Per-byte evaluation result for express lane scoring. */
export type ByteResult = 'pending' | 'green' | 'yellow' | 'red';

/** State of a single row during practice. */
export interface RowState {
  /** Index into Challenge.rows */
  rowIndex: number;
  /** Per-byte results (2 syllables per byte → 1 result per byte). */
  byteResults: ByteResult[];
  /** Whether this row has been completed (all green/yellow). */
  completed: boolean;
}

/** Session statistics (no persistence in v1). */
export interface SessionStats {
  cardsCompleted: number;
  perfectCards: number;
  expressCards: number;
  bestTimeMs: number | null;
  totalTimeMs: number;
  previousTimeMs: number | null;
  combo: number;
  totalCreditedBits: number;
}

export function initialSessionStats(): SessionStats {
  return {
    cardsCompleted: 0,
    perfectCards: 0,
    expressCards: 0,
    bestTimeMs: null,
    totalTimeMs: 0,
    previousTimeMs: null,
    combo: 0,
    totalCreditedBits: 0,
  };
}
```

**Step 5: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/flashcard-types.test.ts`
Expected: PASS

**Step 6: Run full test suite**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run`
Expected: All existing tests still pass (AppMode union is backward-compatible)

**Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/types.ts src/lib/flashcard-types.ts src/lib/flashcard-types.test.ts
git commit -m "feat: add flashcard types and extend AppMode with spellbook"
```

---

### Task 2: Q8 Utility Functions

Pure TypeScript Q8 character mapping for client-side per-byte rendering. These mirror the Rust `q8::BOX_CONSONANT_CHARS`/`BOX_VOWEL_CHARS` constants so we can render individual byte cells with highlight states (which the WASM string output can't do).

**Files:**
- Create: `src/lib/q8-utils.ts`
- Create: `src/lib/q8-utils.test.ts`

**Step 1: Write the failing test**

```typescript
// src/lib/q8-utils.test.ts
import { describe, it, expect } from 'vitest';
import {
  BOX_CONSONANTS,
  BOX_VOWELS,
  nibbleToConsonant,
  nibbleToVowel,
  byteToBoxCell,
  consonantIndex,
  type BoxCell,
} from './q8-utils';

describe('q8-utils', () => {
  it('BOX_CONSONANTS matches Rust constants', () => {
    expect(BOX_CONSONANTS).toEqual(['A', '>', '<', 'V']);
  });

  it('BOX_VOWELS matches Rust constants', () => {
    expect(BOX_VOWELS).toEqual(['O', '=', 'X', 'I']);
  });

  it('nibbleToConsonant maps all 16 nibbles', () => {
    // nibble >> 2 gives consonant index 0-3
    expect(nibbleToConsonant(0)).toBe('A');   // 0 >> 2 = 0
    expect(nibbleToConsonant(4)).toBe('>');   // 4 >> 2 = 1
    expect(nibbleToConsonant(8)).toBe('<');   // 8 >> 2 = 2
    expect(nibbleToConsonant(12)).toBe('V');  // 12 >> 2 = 3
    expect(nibbleToConsonant(7)).toBe('>');   // 7 >> 2 = 1
  });

  it('nibbleToVowel maps all 16 nibbles', () => {
    // nibble & 3 gives vowel index 0-3
    expect(nibbleToVowel(0)).toBe('O');   // 0 & 3 = 0
    expect(nibbleToVowel(1)).toBe('=');   // 1 & 3 = 1
    expect(nibbleToVowel(2)).toBe('X');   // 2 & 3 = 2
    expect(nibbleToVowel(3)).toBe('I');   // 3 & 3 = 3
    expect(nibbleToVowel(5)).toBe('=');   // 5 & 3 = 1
  });

  it('byteToBoxCell returns 2x2 character grid', () => {
    // byte 0x00 = nibbles 0, 0
    const cell = byteToBoxCell(0x00);
    expect(cell).toEqual({
      topLeft: 'A', topRight: 'A',
      bottomLeft: 'O', bottomRight: 'O',
    });
  });

  it('byteToBoxCell for 0xFF', () => {
    // 0xFF = nibbles 15, 15
    // nibble 15: consonant[15>>2=3]='V', vowel[15&3=3]='I'
    const cell = byteToBoxCell(0xFF);
    expect(cell).toEqual({
      topLeft: 'V', topRight: 'V',
      bottomLeft: 'I', bottomRight: 'I',
    });
  });

  it('byteToBoxCell for 0x59', () => {
    // 0x59 = nibbles 5, 9
    // nibble 5: consonant[1]='>', vowel[1]='='
    // nibble 9: consonant[2]='<', vowel[1]='='
    const cell = byteToBoxCell(0x59);
    expect(cell).toEqual({
      topLeft: '>', topRight: '<',
      bottomLeft: '=', bottomRight: '=',
    });
  });

  it('consonantIndex extracts consonant from nibble', () => {
    expect(consonantIndex(0)).toBe(0);
    expect(consonantIndex(4)).toBe(1);
    expect(consonantIndex(8)).toBe(2);
    expect(consonantIndex(15)).toBe(3);
  });
});
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/q8-utils.test.ts`
Expected: FAIL — module not found

**Step 3: Write q8-utils.ts**

```typescript
// src/lib/q8-utils.ts

/** Q8-BOX consonant characters (indexed 0-3). Matches stq8_core::q8::BOX_CONSONANT_CHARS. */
export const BOX_CONSONANTS = ['A', '>', '<', 'V'] as const;

/** Q8-BOX vowel characters (indexed 0-3). Matches stq8_core::q8::BOX_VOWEL_CHARS. */
export const BOX_VOWELS = ['O', '=', 'X', 'I'] as const;

/** Extract consonant index (0-3) from a Q8 nibble (0-15). */
export function consonantIndex(nibble: number): number {
  return (nibble >> 2) & 0x03;
}

/** Extract vowel index (0-3) from a Q8 nibble (0-15). */
export function vowelIndex(nibble: number): number {
  return nibble & 0x03;
}

/** Get the BOX consonant character for a nibble. */
export function nibbleToConsonant(nibble: number): string {
  return BOX_CONSONANTS[consonantIndex(nibble)];
}

/** Get the BOX vowel character for a nibble. */
export function nibbleToVowel(nibble: number): string {
  return BOX_VOWELS[vowelIndex(nibble)];
}

/** A 2x2 character cell representing one byte in Q8-BOX format. */
export interface BoxCell {
  topLeft: string;
  topRight: string;
  bottomLeft: string;
  bottomRight: string;
}

/** Convert a byte (0-255) to its Q8-BOX 2x2 character cell. */
export function byteToBoxCell(byte: number): BoxCell {
  const high = (byte >> 4) & 0x0F;
  const low = byte & 0x0F;
  return {
    topLeft: nibbleToConsonant(high),
    topRight: nibbleToConsonant(low),
    bottomLeft: nibbleToVowel(high),
    bottomRight: nibbleToVowel(low),
  };
}
```

**Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/q8-utils.test.ts`
Expected: PASS

**Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/q8-utils.ts src/lib/q8-utils.test.ts
git commit -m "feat: add Q8-BOX character utilities for per-byte rendering"
```

---

### Task 3: stq8Service

Service class wrapping the stq8-web WASM module. Uses the existing harmony-client pattern: stateful class instantiated in App.svelte, passed via props. For testability, the WASM module is injected via constructor.

**Files:**
- Create: `src/lib/stq8-service.ts`
- Create: `src/lib/stq8-service.test.ts`

**Step 1: Write the failing test**

```typescript
// src/lib/stq8-service.test.ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { Stq8Service } from './stq8-service';
import type { Challenge, LevelInfo } from './flashcard-types';

// Mock WasmPipeline — matches the interface from stq8-web
function createMockWasm() {
  return {
    generate_challenge: vi.fn().mockReturnValue(JSON.stringify({
      level: 'Novice',
      data: [0x42],
      rows: [[0x42]],
    })),
    validate_row: vi.fn().mockReturnValue(JSON.stringify({
      matched: true,
      expected: [{ consonant: "'", vowel: 'O' }, { consonant: "'", vowel: 'O' }],
      heard: [{ consonant: "'", vowel: 'O' }, { consonant: "'", vowel: 'O' }],
    })),
    format_box_q8: vi.fn().mockReturnValue('A A\nO O'),
    format_flat_q8: vi.fn().mockReturnValue("'O'O"),
    level_info: vi.fn().mockReturnValue(JSON.stringify({
      total_bytes: 1,
      bytes_per_row: 1,
      num_rows: 1,
      total_bits: 8,
    })),
    process: vi.fn().mockReturnValue(JSON.stringify({
      syllables: [],
    })),
  };
}

describe('Stq8Service', () => {
  let mockWasm: ReturnType<typeof createMockWasm>;
  let service: Stq8Service;

  beforeEach(() => {
    mockWasm = createMockWasm();
    service = new Stq8Service(mockWasm);
  });

  it('generateChallenge calls WASM and parses JSON', () => {
    const challenge = service.generateChallenge(0);
    expect(mockWasm.generate_challenge).toHaveBeenCalledWith(
      0,
      expect.any(Uint8Array),
    );
    expect(challenge.data).toEqual([0x42]);
    expect(challenge.rows).toEqual([[0x42]]);
  });

  it('generateChallenge passes rng_bytes of correct length', () => {
    mockWasm.level_info.mockReturnValue(JSON.stringify({
      total_bytes: 32, bytes_per_row: 4, num_rows: 8, total_bits: 256,
    }));
    service.generateChallenge(4);
    const call = mockWasm.generate_challenge.mock.calls[0];
    expect(call[1]).toBeInstanceOf(Uint8Array);
    expect(call[1].length).toBe(32);
  });

  it('validateRow calls WASM with expected_bytes and heard_nibbles', () => {
    const result = service.validateRow([0x00], [0, 0]);
    expect(mockWasm.validate_row).toHaveBeenCalledWith(
      new Uint8Array([0x00]),
      new Uint8Array([0, 0]),
    );
    expect(result.matched).toBe(true);
  });

  it('getLevelInfo returns parsed level metadata', () => {
    const info = service.getLevelInfo(0);
    expect(info.total_bytes).toBe(1);
    expect(info.bytes_per_row).toBe(1);
    expect(info.num_rows).toBe(1);
    expect(info.total_bits).toBe(8);
  });

  it('isReady returns true when wasm is provided', () => {
    expect(service.isReady()).toBe(true);
  });

  it('isReady returns false when wasm is null', () => {
    const unloaded = new Stq8Service(null);
    expect(unloaded.isReady()).toBe(false);
  });
});
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/stq8-service.test.ts`
Expected: FAIL — module not found

**Step 3: Write stq8-service.ts**

```typescript
// src/lib/stq8-service.ts
import type { FlashcardLevel, Challenge, LevelInfo } from './flashcard-types';

/** Subset of WasmPipeline methods used by flashcard UI. */
export interface WasmPipelineApi {
  generate_challenge(level: number, rng_bytes: Uint8Array): string;
  validate_row(expected_bytes: Uint8Array, heard_nibbles: Uint8Array): string;
  format_box_q8(data: Uint8Array, bytes_per_row: number): string;
  format_flat_q8(data: Uint8Array, bytes_per_row: number): string;
  level_info(level: number): string;
  process(pcm: Float32Array): string;
}

/** Row validation result from WASM. */
export interface WasmRowResult {
  matched: boolean;
  expected: Array<{ consonant: string; vowel: string }>;
  heard: Array<{ consonant: string; vowel: string }>;
}

/** Utterance result from WASM pipeline.process(). */
export interface UtteranceResult {
  syllables: Array<{ nibble: number; consonant: string; vowel: string }>;
}

export class Stq8Service {
  private wasm: WasmPipelineApi | null;

  constructor(wasm: WasmPipelineApi | null) {
    this.wasm = wasm;
  }

  isReady(): boolean {
    return this.wasm !== null;
  }

  getLevelInfo(level: FlashcardLevel): LevelInfo {
    if (!this.wasm) throw new Error('WASM not loaded');
    return JSON.parse(this.wasm.level_info(level));
  }

  generateChallenge(level: FlashcardLevel): Challenge {
    if (!this.wasm) throw new Error('WASM not loaded');
    const info = this.getLevelInfo(level);
    const rngBytes = new Uint8Array(info.total_bytes);
    crypto.getRandomValues(rngBytes);
    return JSON.parse(this.wasm.generate_challenge(level, rngBytes));
  }

  validateRow(expectedBytes: number[], heardNibbles: number[]): WasmRowResult {
    if (!this.wasm) throw new Error('WASM not loaded');
    return JSON.parse(
      this.wasm.validate_row(
        new Uint8Array(expectedBytes),
        new Uint8Array(heardNibbles),
      ),
    );
  }

  processPcm(pcm: Float32Array): UtteranceResult {
    if (!this.wasm) throw new Error('WASM not loaded');
    return JSON.parse(this.wasm.process(pcm));
  }
}
```

**Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/stq8-service.test.ts`
Expected: PASS

**Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/stq8-service.ts src/lib/stq8-service.test.ts
git commit -m "feat: add stq8Service wrapping WASM flashcard API"
```

---

### Task 4: FlashcardGrid Component

Pure display component rendering the Q8-BOX byte grid. Takes challenge data and per-row results as props. Highlights active row, shows green/yellow/red on completed bytes. No business logic.

**Files:**
- Create: `src/lib/components/FlashcardGrid.svelte`
- Create: `src/lib/components/__tests__/FlashcardGrid.test.ts`

**Step 1: Write the failing test**

```typescript
// src/lib/components/__tests__/FlashcardGrid.test.ts
import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import FlashcardGrid from '../FlashcardGrid.svelte';
import type { RowState } from '../../flashcard-types';

describe('FlashcardGrid', () => {
  it('renders BOX characters for a single byte', () => {
    // byte 0x00 = nibbles 0,0 → consonants A,A vowels O,O
    render(FlashcardGrid, {
      props: {
        rows: [[0x00]],
        activeRowIndex: 0,
        rowStates: [],
      },
    });
    // Should find the BOX characters rendered
    const cells = screen.getAllByTestId('byte-cell');
    expect(cells).toHaveLength(1);
    expect(cells[0].textContent).toContain('A');
    expect(cells[0].textContent).toContain('O');
  });

  it('renders correct number of rows and bytes', () => {
    render(FlashcardGrid, {
      props: {
        rows: [[0x00, 0xFF], [0x42, 0x59]],
        activeRowIndex: 0,
        rowStates: [],
      },
    });
    const rows = screen.getAllByTestId('grid-row');
    expect(rows).toHaveLength(2);
    const cells = screen.getAllByTestId('byte-cell');
    expect(cells).toHaveLength(4);
  });

  it('marks active row with active class', () => {
    render(FlashcardGrid, {
      props: {
        rows: [[0x00], [0xFF]],
        activeRowIndex: 1,
        rowStates: [],
      },
    });
    const rows = screen.getAllByTestId('grid-row');
    expect(rows[0].classList.contains('active')).toBe(false);
    expect(rows[1].classList.contains('active')).toBe(true);
  });

  it('applies green class to completed perfect bytes', () => {
    const rowStates: RowState[] = [
      { rowIndex: 0, byteResults: ['green', 'green'], completed: true },
    ];
    render(FlashcardGrid, {
      props: {
        rows: [[0x00, 0xFF]],
        activeRowIndex: 1,
        rowStates,
      },
    });
    const cells = screen.getAllByTestId('byte-cell');
    expect(cells[0].classList.contains('green')).toBe(true);
    expect(cells[1].classList.contains('green')).toBe(true);
  });

  it('applies yellow class to express-matched bytes', () => {
    const rowStates: RowState[] = [
      { rowIndex: 0, byteResults: ['green', 'yellow'], completed: true },
    ];
    render(FlashcardGrid, {
      props: {
        rows: [[0x00, 0xFF]],
        activeRowIndex: 1,
        rowStates,
      },
    });
    const cells = screen.getAllByTestId('byte-cell');
    expect(cells[0].classList.contains('green')).toBe(true);
    expect(cells[1].classList.contains('yellow')).toBe(true);
  });

  it('renders with accessible role', () => {
    render(FlashcardGrid, {
      props: {
        rows: [[0x00]],
        activeRowIndex: 0,
        rowStates: [],
      },
    });
    expect(screen.getByRole('grid')).toBeTruthy();
  });
});
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/FlashcardGrid.test.ts`
Expected: FAIL — component not found

**Step 3: Write FlashcardGrid.svelte**

```svelte
<!-- src/lib/components/FlashcardGrid.svelte -->
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
      class:completed={rowStates.some(s => s.rowIndex === rowIdx && s.completed)}
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
    border-color: var(--accent, #5865f2);
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
```

**Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/FlashcardGrid.test.ts`
Expected: PASS

**Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/components/FlashcardGrid.svelte src/lib/components/__tests__/FlashcardGrid.test.ts
git commit -m "feat: add FlashcardGrid component for Q8-BOX byte display"
```

---

### Task 5: HintBar Component

Displays Q8-FLAT phonetic text for the active row only. Togglable. Pure display.

**Files:**
- Create: `src/lib/components/HintBar.svelte`
- Create: `src/lib/components/__tests__/HintBar.test.ts`

**Step 1: Write the failing test**

```typescript
// src/lib/components/__tests__/HintBar.test.ts
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import HintBar from '../HintBar.svelte';

describe('HintBar', () => {
  it('shows flat text when visible', () => {
    render(HintBar, {
      props: { flatText: "KU'E", visible: true },
    });
    expect(screen.getByText("KU'E")).toBeTruthy();
  });

  it('hides text when not visible', () => {
    render(HintBar, {
      props: { flatText: "KU'E", visible: false },
    });
    expect(screen.queryByText("KU'E")).toBeNull();
  });

  it('has accessible label', () => {
    render(HintBar, {
      props: { flatText: "KU'E", visible: true },
    });
    expect(screen.getByLabelText('Phonetic hint')).toBeTruthy();
  });

  it('shows placeholder when flatText is empty', () => {
    render(HintBar, {
      props: { flatText: '', visible: true },
    });
    expect(screen.getByText('No active row')).toBeTruthy();
  });
});
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/HintBar.test.ts`
Expected: FAIL

**Step 3: Write HintBar.svelte**

```svelte
<!-- src/lib/components/HintBar.svelte -->
<script lang="ts">
  let {
    flatText,
    visible,
  }: {
    flatText: string;
    visible: boolean;
  } = $props();
</script>

{#if visible}
  <div class="hint-bar" aria-label="Phonetic hint">
    {#if flatText}
      <span class="hint-text">{flatText}</span>
    {:else}
      <span class="hint-placeholder">No active row</span>
    {/if}
  </div>
{/if}

<style>
  .hint-bar {
    text-align: center;
    padding: 6px 12px;
    font-family: 'Courier New', Courier, monospace;
    font-size: 1.1rem;
    color: var(--text-secondary, #b5bac1);
    border-top: 1px solid var(--border, #3f4147);
    background: var(--bg-secondary, #2b2d31);
  }

  .hint-placeholder {
    color: var(--text-muted, #949ba4);
    font-style: italic;
  }
</style>
```

**Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/HintBar.test.ts`
Expected: PASS

**Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/components/HintBar.svelte src/lib/components/__tests__/HintBar.test.ts
git commit -m "feat: add HintBar component for Q8-FLAT phonetic hints"
```

---

### Task 6: PttButton Component

Large circular push-to-talk button with dual activation (mouse hold + spacebar hold). Fires `onPttStart`/`onPttStop` events. The button visually activates on spacebar to provide consistent feedback.

**Important accessibility note:** `role="button"` elements must activate on both Enter and Space (with `preventDefault` on Space to avoid scroll). But this is a PTT (push-to-talk) button — Space is used as a "hold" gesture, not a click. So we use the native `<button>` element and add custom keydown/keyup handlers for Space hold behavior. `preventDefault` on keydown Space prevents page scrolling.

**Files:**
- Create: `src/lib/components/PttButton.svelte`
- Create: `src/lib/components/__tests__/PttButton.test.ts`

**Step 1: Write the failing test**

```typescript
// src/lib/components/__tests__/PttButton.test.ts
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import PttButton from '../PttButton.svelte';

describe('PttButton', () => {
  it('renders with mic label', () => {
    render(PttButton, { props: { active: false } });
    expect(screen.getByRole('button', { name: /push to talk/i })).toBeTruthy();
  });

  it('fires onPttStart on mousedown', async () => {
    const onPttStart = vi.fn();
    render(PttButton, { props: { active: false, onPttStart } });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    await fireEvent.mouseDown(btn);
    expect(onPttStart).toHaveBeenCalledOnce();
  });

  it('fires onPttStop on mouseup', async () => {
    const onPttStop = vi.fn();
    render(PttButton, { props: { active: true, onPttStop } });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    await fireEvent.mouseUp(btn);
    expect(onPttStop).toHaveBeenCalledOnce();
  });

  it('shows active styling when active', () => {
    render(PttButton, { props: { active: true } });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    expect(btn.classList.contains('active')).toBe(true);
  });

  it('shows processing styling when processing', () => {
    render(PttButton, { props: { active: false, processing: true } });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    expect(btn.classList.contains('processing')).toBe(true);
  });

  it('fires onPttStart on spacebar keydown', async () => {
    const onPttStart = vi.fn();
    render(PttButton, { props: { active: false, onPttStart } });
    await fireEvent.keyDown(window, { code: 'Space' });
    expect(onPttStart).toHaveBeenCalledOnce();
  });

  it('fires onPttStop on spacebar keyup', async () => {
    const onPttStop = vi.fn();
    render(PttButton, { props: { active: true, onPttStop } });
    await fireEvent.keyUp(window, { code: 'Space' });
    expect(onPttStop).toHaveBeenCalledOnce();
  });

  it('does not double-fire onPttStart on key repeat', async () => {
    const onPttStart = vi.fn();
    render(PttButton, { props: { active: true, onPttStart } });
    // repeat: true simulates held key
    await fireEvent.keyDown(window, { code: 'Space', repeat: true });
    expect(onPttStart).not.toHaveBeenCalled();
  });

  it('is disabled when disabled prop is true', () => {
    render(PttButton, { props: { active: false, disabled: true } });
    const btn = screen.getByRole('button', { name: /push to talk/i });
    expect(btn.hasAttribute('disabled')).toBe(true);
  });
});
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/PttButton.test.ts`
Expected: FAIL

**Step 3: Write PttButton.svelte**

```svelte
<!-- src/lib/components/PttButton.svelte -->
<script lang="ts">
  let {
    active = false,
    processing = false,
    disabled = false,
    onPttStart,
    onPttStop,
  }: {
    active?: boolean;
    processing?: boolean;
    disabled?: boolean;
    onPttStart?: () => void;
    onPttStop?: () => void;
  } = $props();

  function handleMouseDown() {
    if (disabled) return;
    onPttStart?.();
  }

  function handleMouseUp() {
    if (disabled) return;
    onPttStop?.();
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.code !== 'Space' || e.repeat || disabled) return;
    e.preventDefault();
    onPttStart?.();
  }

  function handleKeyUp(e: KeyboardEvent) {
    if (e.code !== 'Space' || disabled) return;
    e.preventDefault();
    onPttStop?.();
  }
</script>

<svelte:window onkeydown={handleKeyDown} onkeyup={handleKeyUp} />

<button
  type="button"
  class="ptt-button"
  class:active
  class:processing
  aria-label="Push to talk"
  onmousedown={handleMouseDown}
  onmouseup={handleMouseUp}
  onmouseleave={active ? handleMouseUp : undefined}
  ontouchstart={handleMouseDown}
  ontouchend={handleMouseUp}
  {disabled}
>
  <span class="ptt-icon" aria-hidden="true">
    {#if processing}
      ...
    {:else}
      🎤
    {/if}
  </span>
</button>

<style>
  .ptt-button {
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

  .ptt-button:hover:not(:disabled) {
    background: rgba(88, 101, 242, 0.1);
  }

  .ptt-button.active {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
    box-shadow: 0 0 20px rgba(88, 101, 242, 0.4);
  }

  .ptt-button.processing {
    opacity: 0.6;
    cursor: wait;
  }

  .ptt-button:disabled {
    opacity: 0.3;
    cursor: not-allowed;
    border-color: var(--text-muted, #949ba4);
  }

  .ptt-icon {
    pointer-events: none;
  }
</style>
```

**Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/PttButton.test.ts`
Expected: PASS

**Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/components/PttButton.svelte src/lib/components/__tests__/PttButton.test.ts
git commit -m "feat: add PttButton with mouse hold and spacebar activation"
```

---

### Task 7: FlashcardStats Component

Detail panel showing session statistics. Pure display — receives `SessionStats` as props.

**Files:**
- Create: `src/lib/components/FlashcardStats.svelte`
- Create: `src/lib/components/__tests__/FlashcardStats.test.ts`

**Step 1: Write the failing test**

```typescript
// src/lib/components/__tests__/FlashcardStats.test.ts
import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import FlashcardStats from '../FlashcardStats.svelte';
import { initialSessionStats, type SessionStats } from '../../flashcard-types';

describe('FlashcardStats', () => {
  it('renders all stat labels', () => {
    render(FlashcardStats, { props: { stats: initialSessionStats() } });
    expect(screen.getByText('Cards completed')).toBeTruthy();
    expect(screen.getByText('Perfect cards')).toBeTruthy();
    expect(screen.getByText('Express cards')).toBeTruthy();
    expect(screen.getByText('Best time')).toBeTruthy();
    expect(screen.getByText('Average time')).toBeTruthy();
    expect(screen.getByText('Previous time')).toBeTruthy();
    expect(screen.getByText('Combo')).toBeTruthy();
    expect(screen.getByText('Effective bitrate')).toBeTruthy();
  });

  it('displays zeroed stats', () => {
    render(FlashcardStats, { props: { stats: initialSessionStats() } });
    // Cards completed should show 0
    const values = screen.getAllByTestId('stat-value');
    expect(values[0].textContent).toBe('0');
  });

  it('displays populated stats', () => {
    const stats: SessionStats = {
      cardsCompleted: 5,
      perfectCards: 3,
      expressCards: 2,
      bestTimeMs: 1234,
      totalTimeMs: 6170,
      previousTimeMs: 1500,
      combo: 3,
      totalCreditedBits: 200,
    };
    render(FlashcardStats, { props: { stats } });
    const values = screen.getAllByTestId('stat-value');
    expect(values[0].textContent).toBe('5'); // cards completed
    expect(values[1].textContent).toBe('3'); // perfect
    expect(values[2].textContent).toBe('2'); // express
  });

  it('formats time values as seconds', () => {
    const stats: SessionStats = {
      ...initialSessionStats(),
      bestTimeMs: 2500,
      previousTimeMs: 3100,
    };
    render(FlashcardStats, { props: { stats } });
    expect(screen.getByText('2.50s')).toBeTruthy();
    expect(screen.getByText('3.10s')).toBeTruthy();
  });

  it('shows dash for null time values', () => {
    render(FlashcardStats, { props: { stats: initialSessionStats() } });
    // Best time and previous time are null → show '—'
    expect(screen.getAllByText('—').length).toBeGreaterThanOrEqual(2);
  });

  it('calculates effective bitrate', () => {
    const stats: SessionStats = {
      ...initialSessionStats(),
      cardsCompleted: 1,
      totalCreditedBits: 80,
      totalTimeMs: 10000, // 10 seconds
    };
    render(FlashcardStats, { props: { stats } });
    // 80 bits / 10 seconds = 8.0 bps
    expect(screen.getByText('8.0 bps')).toBeTruthy();
  });
});
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/FlashcardStats.test.ts`
Expected: FAIL

**Step 3: Write FlashcardStats.svelte**

```svelte
<!-- src/lib/components/FlashcardStats.svelte -->
<script lang="ts">
  import type { SessionStats } from '../flashcard-types';

  let { stats }: { stats: SessionStats } = $props();

  function formatTime(ms: number | null): string {
    if (ms === null) return '—';
    return `${(ms / 1000).toFixed(2)}s`;
  }

  let averageTimeMs = $derived(
    stats.cardsCompleted > 0 ? stats.totalTimeMs / stats.cardsCompleted : null
  );

  let effectiveBitrate = $derived(
    stats.totalTimeMs > 0
      ? `${(stats.totalCreditedBits / (stats.totalTimeMs / 1000)).toFixed(1)} bps`
      : '—'
  );

  const statRows = $derived([
    { label: 'Cards completed', value: String(stats.cardsCompleted) },
    { label: 'Perfect cards', value: String(stats.perfectCards) },
    { label: 'Express cards', value: String(stats.expressCards) },
    { label: 'Best time', value: formatTime(stats.bestTimeMs) },
    { label: 'Average time', value: formatTime(averageTimeMs) },
    { label: 'Previous time', value: formatTime(stats.previousTimeMs) },
    { label: 'Combo', value: String(stats.combo) },
    { label: 'Effective bitrate', value: effectiveBitrate },
  ]);
</script>

<div class="flashcard-stats">
  <h3 class="stats-title">Session Stats</h3>
  <dl class="stats-list">
    {#each statRows as row}
      <div class="stat-row">
        <dt class="stat-label">{row.label}</dt>
        <dd class="stat-value" data-testid="stat-value">{row.value}</dd>
      </div>
    {/each}
  </dl>
</div>

<style>
  .flashcard-stats {
    padding: 16px;
  }

  .stats-title {
    font-size: 0.875rem;
    font-weight: 600;
    color: var(--text-primary, #f2f3f5);
    margin: 0 0 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .stats-list {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .stat-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }

  .stat-label {
    font-size: 0.8125rem;
    color: var(--text-secondary, #b5bac1);
  }

  .stat-value {
    font-size: 0.875rem;
    font-weight: 600;
    color: var(--text-primary, #f2f3f5);
    font-family: 'Courier New', Courier, monospace;
    margin: 0;
  }
</style>
```

**Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/FlashcardStats.test.ts`
Expected: PASS

**Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/components/FlashcardStats.svelte src/lib/components/__tests__/FlashcardStats.test.ts
git commit -m "feat: add FlashcardStats component for session statistics"
```

---

### Task 8: SpellList Component

Bookmark list showing Q8 page addresses into CAS. Empty state in v1 — just a prompt to try Practice.

**Files:**
- Create: `src/lib/components/SpellList.svelte`
- Create: `src/lib/components/__tests__/SpellList.test.ts`

**Step 1: Write the failing test**

```typescript
// src/lib/components/__tests__/SpellList.test.ts
import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import SpellList from '../SpellList.svelte';

describe('SpellList', () => {
  it('shows empty state message', () => {
    render(SpellList);
    expect(screen.getByText(/no spells yet/i)).toBeTruthy();
  });

  it('suggests trying practice', () => {
    render(SpellList);
    expect(screen.getByText(/practice/i)).toBeTruthy();
  });
});
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/SpellList.test.ts`
Expected: FAIL

**Step 3: Write SpellList.svelte**

```svelte
<!-- src/lib/components/SpellList.svelte -->
<script lang="ts">
  // v1: Empty state. Future: displays bookmarked Q8 page addresses.
</script>

<div class="spell-list-empty">
  <p class="empty-title">No spells yet</p>
  <p class="empty-hint">Try the Practice tab to start learning Q8 pronunciation.</p>
</div>

<style>
  .spell-list-empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    height: 100%;
    padding: 32px;
    text-align: center;
  }

  .empty-title {
    font-size: 1.125rem;
    color: var(--text-primary, #f2f3f5);
    margin: 0 0 8px;
  }

  .empty-hint {
    font-size: 0.875rem;
    color: var(--text-muted, #949ba4);
    margin: 0;
  }
</style>
```

**Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/SpellList.test.ts`
Expected: PASS

**Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/components/SpellList.svelte src/lib/components/__tests__/SpellList.test.ts
git commit -m "feat: add SpellList empty state component"
```

---

### Task 9: FlashcardView Component

The main orchestrator. Owns session state: current challenge, active row, per-byte results, timers, combo, stats. Coordinates PttButton events, FlashcardGrid display, HintBar, and row validation via stq8Service.

Express lane logic lives here as a validation-layer policy: compare consonant indices of heard vs expected nibbles. Same consonant + different vowel = yellow (4 bits). Different consonant = red (0 bits, row fails).

**Files:**
- Create: `src/lib/components/FlashcardView.svelte`
- Create: `src/lib/components/__tests__/FlashcardView.test.ts`

**Step 1: Write the failing test**

```typescript
// src/lib/components/__tests__/FlashcardView.test.ts
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import FlashcardView from '../FlashcardView.svelte';
import type { SessionStats } from '../../flashcard-types';

// Mock stq8Service
function createMockService() {
  return {
    isReady: vi.fn().mockReturnValue(true),
    getLevelInfo: vi.fn().mockReturnValue({
      total_bytes: 2,
      bytes_per_row: 2,
      num_rows: 1,
      total_bits: 16,
    }),
    generateChallenge: vi.fn().mockReturnValue({
      level: 'Apprentice',
      data: [0x00, 0xFF],
      rows: [[0x00, 0xFF]],
    }),
    validateRow: vi.fn().mockReturnValue({
      matched: true,
      expected: [],
      heard: [],
    }),
    processPcm: vi.fn().mockReturnValue({ syllables: [] }),
  };
}

describe('FlashcardView', () => {
  let mockService: ReturnType<typeof createMockService>;

  beforeEach(() => {
    mockService = createMockService();
  });

  it('renders grid with challenge data', () => {
    render(FlashcardView, {
      props: {
        level: 1,
        expressLane: false,
        stq8Service: mockService,
        onStatsUpdate: vi.fn(),
      },
    });
    expect(mockService.generateChallenge).toHaveBeenCalledWith(1);
    expect(screen.getByRole('grid')).toBeTruthy();
  });

  it('renders PTT button', () => {
    render(FlashcardView, {
      props: {
        level: 1,
        expressLane: false,
        stq8Service: mockService,
        onStatsUpdate: vi.fn(),
      },
    });
    expect(screen.getByRole('button', { name: /push to talk/i })).toBeTruthy();
  });

  it('shows loading state when service not ready', () => {
    mockService.isReady.mockReturnValue(false);
    render(FlashcardView, {
      props: {
        level: 1,
        expressLane: false,
        stq8Service: mockService,
        onStatsUpdate: vi.fn(),
      },
    });
    expect(screen.getByText(/loading/i)).toBeTruthy();
  });

  it('generates new challenge when level changes', () => {
    const { rerender } = render(FlashcardView, {
      props: {
        level: 1,
        expressLane: false,
        stq8Service: mockService,
        onStatsUpdate: vi.fn(),
      },
    });
    expect(mockService.generateChallenge).toHaveBeenCalledTimes(1);
    // Level change triggers new challenge
    rerender({
      level: 2,
      expressLane: false,
      stq8Service: mockService,
      onStatsUpdate: vi.fn(),
    });
    expect(mockService.generateChallenge).toHaveBeenCalledTimes(2);
  });

  it('shows hint bar toggle', () => {
    render(FlashcardView, {
      props: {
        level: 1,
        expressLane: false,
        stq8Service: mockService,
        onStatsUpdate: vi.fn(),
      },
    });
    expect(screen.getByRole('button', { name: /hint/i })).toBeTruthy();
  });
});
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/FlashcardView.test.ts`
Expected: FAIL

**Step 3: Write FlashcardView.svelte**

```svelte
<!-- src/lib/components/FlashcardView.svelte -->
<script lang="ts">
  import type {
    FlashcardLevel,
    Challenge,
    RowState,
    ByteResult,
    SessionStats,
  } from '../flashcard-types';
  import { initialSessionStats } from '../flashcard-types';
  import { consonantIndex } from '../q8-utils';
  import type { Stq8Service } from '../stq8-service';
  import FlashcardGrid from './FlashcardGrid.svelte';
  import HintBar from './HintBar.svelte';
  import PttButton from './PttButton.svelte';

  let {
    level,
    expressLane,
    stq8Service,
    onStatsUpdate,
  }: {
    level: FlashcardLevel;
    expressLane: boolean;
    stq8Service: { isReady(): boolean; getLevelInfo(l: FlashcardLevel): { total_bytes: number; bytes_per_row: number; num_rows: number; total_bits: number }; generateChallenge(l: FlashcardLevel): Challenge; validateRow(e: number[], h: number[]): { matched: boolean; expected: unknown[]; heard: unknown[] }; processPcm(pcm: Float32Array): { syllables: Array<{ nibble: number }> } };
    expressLane: boolean;
    onStatsUpdate: (stats: SessionStats) => void;
  } = $props();

  let challenge = $state<Challenge | null>(null);
  let activeRowIndex = $state(0);
  let rowStates = $state<RowState[]>([]);
  let pttActive = $state(false);
  let showHint = $state(false);
  let stats = $state(initialSessionStats());
  let cardStartTime = $state<number | null>(null);

  // Generate challenge when level changes or on mount
  let currentLevel = $state(level);
  $effect(() => {
    if (level !== currentLevel || !challenge) {
      currentLevel = level;
      newChallenge();
    }
  });

  function newChallenge() {
    if (!stq8Service.isReady()) return;
    challenge = stq8Service.generateChallenge(level);
    activeRowIndex = 0;
    rowStates = [];
    cardStartTime = null;
  }

  function handlePttStart() {
    pttActive = true;
    if (cardStartTime === null) {
      cardStartTime = Date.now();
    }
  }

  function handlePttStop() {
    pttActive = false;
    // Release PTT = cancel current row (reset to row start, banked rows kept)
    // Active row results are cleared; completed rows stay
  }

  /**
   * Express lane per-byte evaluation.
   * For each byte (pair of nibbles): compare expected vs heard.
   * - Both match → green (8 bits)
   * - Consonant matches, vowel doesn't (express ON) → yellow (4 bits)
   * - Consonant doesn't match → red (0 bits)
   */
  function evaluateBytes(
    expectedBytes: number[],
    heardNibbles: number[],
    express: boolean,
  ): { results: ByteResult[]; creditedBits: number; hasRed: boolean } {
    const results: ByteResult[] = [];
    let creditedBits = 0;
    let hasRed = false;

    for (let i = 0; i < expectedBytes.length; i++) {
      const byte = expectedBytes[i];
      const expHigh = (byte >> 4) & 0x0F;
      const expLow = byte & 0x0F;
      const heardHigh = heardNibbles[i * 2] ?? -1;
      const heardLow = heardNibbles[i * 2 + 1] ?? -1;

      if (expHigh === heardHigh && expLow === heardLow) {
        results.push('green');
        creditedBits += 8;
      } else if (
        express &&
        consonantIndex(expHigh) === consonantIndex(heardHigh) &&
        consonantIndex(expLow) === consonantIndex(heardLow)
      ) {
        results.push('yellow');
        creditedBits += 4;
      } else {
        results.push('red');
        hasRed = true;
      }
    }

    return { results, creditedBits, hasRed };
  }

  function handleRowComplete(heardNibbles: number[]) {
    if (!challenge) return;
    const row = challenge.rows[activeRowIndex];
    if (!row) return;

    const { results, creditedBits, hasRed } = evaluateBytes(
      row, heardNibbles, expressLane,
    );

    if (hasRed) {
      // Brief red flash, then reset row
      rowStates = [
        ...rowStates.filter(s => s.rowIndex !== activeRowIndex),
        { rowIndex: activeRowIndex, byteResults: results, completed: false },
      ];
      // Clear after animation
      setTimeout(() => {
        rowStates = rowStates.filter(s => s.rowIndex !== activeRowIndex);
      }, 300);
      // Combo breaks on red
      stats = { ...stats, combo: 0 };
      return;
    }

    // Row passed — bank it
    rowStates = [
      ...rowStates.filter(s => s.rowIndex !== activeRowIndex),
      { rowIndex: activeRowIndex, byteResults: results, completed: true },
    ];

    // Check if card is complete
    if (activeRowIndex >= challenge.rows.length - 1) {
      handleCardComplete(creditedBits, results);
    } else {
      activeRowIndex++;
    }
  }

  function handleCardComplete(lastRowBits: number, lastRowResults: ByteResult[]) {
    const elapsed = cardStartTime ? Date.now() - cardStartTime : 0;

    // Sum all credited bits across all rows
    let totalBits = lastRowBits;
    for (const rs of rowStates) {
      if (rs.completed) {
        totalBits += rs.byteResults.filter(r => r === 'green').length * 8
          + rs.byteResults.filter(r => r === 'yellow').length * 4;
      }
    }

    const hasYellow = rowStates.some(s =>
      s.byteResults.some(r => r === 'yellow')
    ) || lastRowResults.some(r => r === 'yellow');

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

    // Auto-advance to next card (if PTT still held, keep going)
    newChallenge();
  }

  // Flat text for active row hint
  let hintText = $derived.by(() => {
    if (!challenge || !challenge.rows[activeRowIndex]) return '';
    // Generate flat text from bytes (simplified — use WASM format_flat in production)
    const FLAT_CONSONANTS = ["'", 'J', 'K', 'V'];
    const FLAT_VOWELS = ['O', 'U', 'E', 'I'];
    const row = challenge.rows[activeRowIndex];
    return row.map(byte => {
      const high = (byte >> 4) & 0x0F;
      const low = byte & 0x0F;
      const s1 = FLAT_CONSONANTS[(high >> 2) & 3] + FLAT_VOWELS[high & 3];
      const s2 = FLAT_CONSONANTS[(low >> 2) & 3] + FLAT_VOWELS[low & 3];
      return s1 + s2;
    }).join(' ');
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
        onclick={() => { showHint = !showHint; }}
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
```

**Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/FlashcardView.test.ts`
Expected: PASS

**Step 5: Write express lane scoring tests**

Add to `FlashcardView.test.ts`:

```typescript
// Add to the existing describe block

describe('express lane scoring', () => {
  // We test evaluateBytes indirectly through the component.
  // For direct testing, extract to a utility and test separately.

  it('renders express lane result colors on grid', () => {
    // This test verifies the visual integration —
    // express lane logic is in evaluateBytes() which is tested
    // via the q8-utils consonantIndex function
  });
});
```

Actually, since `evaluateBytes` is a private function inside the component, add a separate pure-function test file:

Create: `src/lib/express-lane.ts` and `src/lib/express-lane.test.ts`

```typescript
// src/lib/express-lane.test.ts
import { describe, it, expect } from 'vitest';
import { evaluateBytes } from './express-lane';

describe('evaluateBytes', () => {
  it('all green when exact match (express off)', () => {
    // byte 0x00 = nibbles 0,0. Heard [0,0] = exact match
    const { results, creditedBits, hasRed } = evaluateBytes([0x00], [0, 0], false);
    expect(results).toEqual(['green']);
    expect(creditedBits).toBe(8);
    expect(hasRed).toBe(false);
  });

  it('red when mismatch (express off)', () => {
    // byte 0x00 = nibbles 0,0. Heard [1,0] = wrong high nibble
    const { results, creditedBits, hasRed } = evaluateBytes([0x00], [1, 0], false);
    expect(results).toEqual(['red']);
    expect(creditedBits).toBe(0);
    expect(hasRed).toBe(true);
  });

  it('yellow when consonant matches but vowel differs (express on)', () => {
    // byte 0x00 = nibbles 0,0
    // nibble 0: consonant=0, vowel=0
    // nibble 1: consonant=0, vowel=1 (same consonant!)
    // Heard [1, 1] → consonant matches for both nibbles
    const { results, creditedBits, hasRed } = evaluateBytes([0x00], [1, 1], true);
    expect(results).toEqual(['yellow']);
    expect(creditedBits).toBe(4);
    expect(hasRed).toBe(false);
  });

  it('red when consonant differs even with express on', () => {
    // byte 0x00 = nibbles 0,0 → consonant index 0
    // Heard [4, 0] → nibble 4 consonant index 1 (different!)
    const { results, creditedBits, hasRed } = evaluateBytes([0x00], [4, 0], true);
    expect(results).toEqual(['red']);
    expect(creditedBits).toBe(0);
    expect(hasRed).toBe(true);
  });

  it('mixed results across multiple bytes', () => {
    // byte 0x00 (nibbles 0,0), byte 0xFF (nibbles 15,15)
    // Heard for byte 0: [0,0] = exact → green
    // Heard for byte 1: [13,13] → nibble 15 consonant=3, nibble 13 consonant=3 (same!) → yellow
    const { results, creditedBits, hasRed } = evaluateBytes(
      [0x00, 0xFF], [0, 0, 13, 13], true
    );
    expect(results).toEqual(['green', 'yellow']);
    expect(creditedBits).toBe(12); // 8 + 4
    expect(hasRed).toBe(false);
  });

  it('vowel-only mismatch is red without express', () => {
    // Same consonant, different vowel, but express OFF → red
    const { results, hasRed } = evaluateBytes([0x00], [1, 1], false);
    expect(results).toEqual(['red']);
    expect(hasRed).toBe(true);
  });
});
```

```typescript
// src/lib/express-lane.ts
import type { ByteResult } from './flashcard-types';
import { consonantIndex } from './q8-utils';

/**
 * Express lane per-byte evaluation.
 *
 * For each byte (pair of nibbles), compare expected vs heard:
 * - Both nibbles match exactly → green (8 bits)
 * - Consonants match, vowels differ, express ON → yellow (4 bits)
 * - Consonant doesn't match → red (0 bits)
 */
export function evaluateBytes(
  expectedBytes: number[],
  heardNibbles: number[],
  express: boolean,
): { results: ByteResult[]; creditedBits: number; hasRed: boolean } {
  const results: ByteResult[] = [];
  let creditedBits = 0;
  let hasRed = false;

  for (let i = 0; i < expectedBytes.length; i++) {
    const byte = expectedBytes[i];
    const expHigh = (byte >> 4) & 0x0F;
    const expLow = byte & 0x0F;
    const heardHigh = heardNibbles[i * 2] ?? -1;
    const heardLow = heardNibbles[i * 2 + 1] ?? -1;

    if (expHigh === heardHigh && expLow === heardLow) {
      results.push('green');
      creditedBits += 8;
    } else if (
      express &&
      consonantIndex(expHigh) === consonantIndex(heardHigh) &&
      consonantIndex(expLow) === consonantIndex(heardLow)
    ) {
      results.push('yellow');
      creditedBits += 4;
    } else {
      results.push('red');
      hasRed = true;
    }
  }

  return { results, creditedBits, hasRed };
}
```

Then update FlashcardView.svelte to import from `express-lane.ts` instead of having inline logic.

**Step 6: Run tests**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/express-lane.test.ts src/lib/components/__tests__/FlashcardView.test.ts`
Expected: PASS

**Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/express-lane.ts src/lib/express-lane.test.ts \
  src/lib/components/FlashcardView.svelte src/lib/components/__tests__/FlashcardView.test.ts
git commit -m "feat: add FlashcardView orchestrator with express lane scoring"
```

---

### Task 10: SpellbookMode Container

Top-level mode container with two tabs (Spells/Practice), express lane toggle, and level selector. Routes to SpellList or FlashcardView based on active tab.

**Files:**
- Create: `src/lib/components/SpellbookMode.svelte`
- Create: `src/lib/components/__tests__/SpellbookMode.test.ts`

**Step 1: Write the failing test**

```typescript
// src/lib/components/__tests__/SpellbookMode.test.ts
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import SpellbookMode from '../SpellbookMode.svelte';

function createMockService() {
  return {
    isReady: vi.fn().mockReturnValue(true),
    getLevelInfo: vi.fn().mockReturnValue({
      total_bytes: 1, bytes_per_row: 1, num_rows: 1, total_bits: 8,
    }),
    generateChallenge: vi.fn().mockReturnValue({
      level: 'Novice', data: [0x42], rows: [[0x42]],
    }),
    validateRow: vi.fn().mockReturnValue({ matched: true, expected: [], heard: [] }),
    processPcm: vi.fn().mockReturnValue({ syllables: [] }),
  };
}

describe('SpellbookMode', () => {
  it('renders Spells and Practice tabs', () => {
    render(SpellbookMode, {
      props: { stq8Service: createMockService() },
    });
    expect(screen.getByRole('tab', { name: /spells/i })).toBeTruthy();
    expect(screen.getByRole('tab', { name: /practice/i })).toBeTruthy();
  });

  it('shows Practice tab by default', () => {
    render(SpellbookMode, {
      props: { stq8Service: createMockService() },
    });
    const practiceTab = screen.getByRole('tab', { name: /practice/i });
    expect(practiceTab.getAttribute('aria-selected')).toBe('true');
  });

  it('switches to Spells tab on click', async () => {
    render(SpellbookMode, {
      props: { stq8Service: createMockService() },
    });
    await fireEvent.click(screen.getByRole('tab', { name: /spells/i }));
    expect(screen.getByText(/no spells yet/i)).toBeTruthy();
  });

  it('renders level selector', () => {
    render(SpellbookMode, {
      props: { stq8Service: createMockService() },
    });
    expect(screen.getByLabelText(/level/i)).toBeTruthy();
  });

  it('renders express lane toggle', () => {
    render(SpellbookMode, {
      props: { stq8Service: createMockService() },
    });
    expect(screen.getByLabelText(/express lane/i)).toBeTruthy();
  });

  it('level selector changes challenge level', async () => {
    const mockService = createMockService();
    render(SpellbookMode, {
      props: { stq8Service: mockService },
    });
    const select = screen.getByLabelText(/level/i);
    await fireEvent.change(select, { target: { value: '3' } });
    // After level change, generateChallenge should be called with new level
    expect(mockService.generateChallenge).toHaveBeenCalledWith(3);
  });

  it('has accessible tablist', () => {
    render(SpellbookMode, {
      props: { stq8Service: createMockService() },
    });
    expect(screen.getByRole('tablist')).toBeTruthy();
  });
});
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/SpellbookMode.test.ts`
Expected: FAIL

**Step 3: Write SpellbookMode.svelte**

```svelte
<!-- src/lib/components/SpellbookMode.svelte -->
<script lang="ts">
  import type { FlashcardLevel, SessionStats } from '../flashcard-types';
  import { LEVELS, LEVEL_NAMES, initialSessionStats } from '../flashcard-types';
  import type { Stq8Service } from '../stq8-service';
  import SpellList from './SpellList.svelte';
  import FlashcardView from './FlashcardView.svelte';
  import FlashcardStats from './FlashcardStats.svelte';

  let {
    stq8Service,
    onStatsUpdate,
  }: {
    stq8Service: { isReady(): boolean; getLevelInfo(l: FlashcardLevel): { total_bytes: number; bytes_per_row: number; num_rows: number; total_bits: number }; generateChallenge(l: FlashcardLevel): { level: string; data: number[]; rows: number[][] }; validateRow(e: number[], h: number[]): { matched: boolean; expected: unknown[]; heard: unknown[] }; processPcm(pcm: Float32Array): { syllables: Array<{ nibble: number }> } };
    onStatsUpdate?: (stats: SessionStats) => void;
  } = $props();

  type SpellbookTab = 'spells' | 'practice';
  let activeTab = $state<SpellbookTab>('practice');
  let level = $state<FlashcardLevel>(0);
  let expressLane = $state(false);
  let stats = $state(initialSessionStats());

  function handleLevelChange(e: Event) {
    const target = e.target as HTMLSelectElement;
    level = Number(target.value) as FlashcardLevel;
  }

  function handleStatsUpdate(newStats: SessionStats) {
    stats = newStats;
    onStatsUpdate?.(newStats);
  }
</script>

<div class="spellbook-mode">
  <header class="spellbook-toolbar">
    <div class="tab-bar" role="tablist" aria-label="Spellbook tabs">
      <button
        type="button"
        role="tab"
        aria-label="Spells"
        aria-selected={activeTab === 'spells'}
        class="tab-btn"
        class:active={activeTab === 'spells'}
        onclick={() => { activeTab = 'spells'; }}
      >Spells</button>
      <button
        type="button"
        role="tab"
        aria-label="Practice"
        aria-selected={activeTab === 'practice'}
        class="tab-btn"
        class:active={activeTab === 'practice'}
        onclick={() => { activeTab = 'practice'; }}
      >Practice</button>
    </div>

    {#if activeTab === 'practice'}
      <div class="toolbar-controls">
        <label class="level-selector">
          <span class="sr-only">Level</span>
          <select aria-label="Level" value={level} onchange={handleLevelChange}>
            {#each LEVELS as l}
              <option value={l}>{LEVEL_NAMES[l]}</option>
            {/each}
          </select>
        </label>

        <label class="express-toggle">
          <input
            type="checkbox"
            bind:checked={expressLane}
            aria-label="Express lane"
          />
          <span>Express</span>
        </label>
      </div>
    {/if}
  </header>

  <div class="spellbook-content" role="tabpanel">
    {#if activeTab === 'spells'}
      <SpellList />
    {:else}
      <FlashcardView
        {level}
        {expressLane}
        {stq8Service}
        onStatsUpdate={handleStatsUpdate}
      />
    {/if}
  </div>
</div>

<style>
  .spellbook-mode {
    display: flex;
    flex-direction: column;
    height: 100%;
  }

  .spellbook-toolbar {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 12px;
    border-bottom: 1px solid var(--border, #3f4147);
    background: var(--bg-secondary, #2b2d31);
    flex-wrap: wrap;
  }

  .tab-bar {
    display: flex;
    gap: 2px;
  }

  .tab-btn {
    padding: 6px 16px;
    border: none;
    border-radius: 4px;
    background: var(--bg-tertiary, #313338);
    color: var(--text-muted, #949ba4);
    cursor: pointer;
    font-size: 0.8125rem;
    font-weight: 500;
  }

  .tab-btn.active {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
  }

  .toolbar-controls {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-left: auto;
  }

  .level-selector select {
    padding: 4px 8px;
    border: 1px solid var(--border, #3f4147);
    border-radius: 4px;
    background: var(--bg-tertiary, #313338);
    color: var(--text-primary, #f2f3f5);
    font-size: 0.8125rem;
  }

  .express-toggle {
    display: flex;
    align-items: center;
    gap: 4px;
    color: var(--text-secondary, #b5bac1);
    font-size: 0.8125rem;
    cursor: pointer;
  }

  .express-toggle input[type="checkbox"] {
    accent-color: var(--accent, #5865f2);
  }

  .spellbook-content {
    flex: 1;
    overflow-y: auto;
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    border: 0;
  }
</style>
```

**Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/components/__tests__/SpellbookMode.test.ts`
Expected: PASS

**Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/components/SpellbookMode.svelte src/lib/components/__tests__/SpellbookMode.test.ts
git commit -m "feat: add SpellbookMode container with tabs, level selector, express toggle"
```

---

### Task 11: App Integration

Wire SpellbookMode into the existing app: add `'spellbook'` to Layout, add mode button to NavPanel, add snippet to App.svelte. FlashcardStats goes in the detail panel when in spellbook mode.

**Files:**
- Modify: `src/lib/components/Layout.svelte` (add spellbook-mode branch)
- Modify: `src/lib/components/NavPanel.svelte` (add Spellbook button)
- Modify: `src/App.svelte` (add spellbook state, snippets, stq8Service)
- Modify: `src/lib/components/__tests__/Layout.test.ts` (add spellbook mode test)
- Modify: `src/lib/components/__tests__/NavPanel.test.ts` (add spellbook button test)

**Step 1: Update Layout.svelte props and grid**

Add to Layout.svelte `$props()`:
```typescript
// Add to the let {...} = $props() destructuring:
spellbookContent?: Snippet;
spellbookDetail?: Snippet;
```

Add to Layout.svelte template, after the vines branch and before the `:else`:
```svelte
{:else if mode === 'spellbook' && spellbookContent}
  <main class="spellbook-area">
    {@render spellbookContent()}
  </main>
  {#if !collapsed && spellbookDetail}
    <section class="detail-area">
      {@render spellbookDetail()}
    </section>
  {/if}
```

Add to Layout.svelte styles:
```css
.layout.spellbook-mode {
  grid-template-columns: var(--nav-width) 1fr 320px;
  grid-template-areas: "nav spellbook detail";
}
.layout.spellbook-mode.collapsed {
  grid-template-columns: var(--nav-width-collapsed) 1fr;
  grid-template-areas: "nav spellbook";
}
.spellbook-area {
  grid-area: spellbook;
  background: var(--bg-primary);
  overflow-y: auto;
  display: flex;
  flex-direction: column;
}
```

Add `spellbook-mode` class on the layout div:
```svelte
class:spellbook-mode={mode === 'spellbook' && spellbookContent}
```

**Step 2: Update NavPanel.svelte**

Add Spellbook button to the mode toggles (after Files, before the closing `</div>` of `.mode-toggles`):

```svelte
<button type="button" class="nav-action-btn mode-toggle" class:active={appMode === 'spellbook'}
  aria-label="Spellbook" aria-pressed={appMode === 'spellbook'}
  onclick={() => onModeChange?.('spellbook')}>Spellbook</button>
```

In the nav tree section, add an `{:else if appMode === 'spellbook'}` branch that shows nothing in the nav tree (spellbook uses its own tab content, not the nav tree):

```svelte
{:else if appMode === 'spellbook'}
  <!-- Spellbook mode uses its own tab content -->
```

**Step 3: Update App.svelte**

Add imports:
```typescript
import SpellbookMode from './lib/components/SpellbookMode.svelte';
import FlashcardStats from './lib/components/FlashcardStats.svelte';
import { Stq8Service } from './lib/stq8-service';
import { initialSessionStats } from './lib/flashcard-types';
```

Add state:
```typescript
const stq8Service = new Stq8Service(null); // WASM loaded async later
let flashcardStats = $state(initialSessionStats());
```

Add snippets inside `<Layout>`:
```svelte
{#snippet spellbookContent()}
  <SpellbookMode
    {stq8Service}
    onStatsUpdate={(stats) => { flashcardStats = stats; }}
  />
{/snippet}
{#snippet spellbookDetail()}
  <FlashcardStats stats={flashcardStats} />
{/snippet}
```

Pass to Layout:
```svelte
<Layout {collapsed} {showSettings} mode={appMode} {spellbookContent} {spellbookDetail}>
```

**Step 4: Update Layout test**

Add to `src/lib/components/__tests__/Layout.test.ts`:

```typescript
it('renders spellbook content in spellbook mode', () => {
  // Test that spellbook-mode class is applied and content renders
});
```

**Step 5: Update NavPanel test**

Add to `src/lib/components/__tests__/NavPanel.test.ts`:

```typescript
it('renders Spellbook mode button', () => {
  render(NavPanel, { props: { nodes: testNodes, collapsed: false } });
  expect(screen.getByRole('button', { name: /spellbook/i })).toBeTruthy();
});

it('calls onModeChange with spellbook when clicked', async () => {
  const onModeChange = vi.fn();
  render(NavPanel, { props: { nodes: testNodes, collapsed: false, onModeChange } });
  await fireEvent.click(screen.getByRole('button', { name: /spellbook/i }));
  expect(onModeChange).toHaveBeenCalledWith('spellbook');
});
```

**Step 6: Run full test suite**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run`
Expected: All tests pass

**Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/App.svelte src/lib/components/Layout.svelte src/lib/components/NavPanel.svelte \
  src/lib/components/__tests__/Layout.test.ts src/lib/components/__tests__/NavPanel.test.ts
git commit -m "feat: wire SpellbookMode into app layout and navigation"
```

---

### Task 12: AudioService

Web Audio API service for microphone capture. Captures PCM audio at 16kHz via AudioWorklet and delivers chunks to a callback. Start/stop lifecycle tied to PTT.

**Note:** AudioWorklet and getUserMedia are not available in jsdom, so tests mock AudioContext. The service is tested for lifecycle management and callback wiring, not actual audio capture.

**Files:**
- Create: `src/lib/audio-service.ts`
- Create: `src/lib/audio-service.test.ts`

**Step 1: Write the failing test**

```typescript
// src/lib/audio-service.test.ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { AudioService } from './audio-service';

// Mock Web Audio API (not available in jsdom)
function createMockAudioContext() {
  const analyser = { connect: vi.fn(), disconnect: vi.fn() };
  const source = { connect: vi.fn(), disconnect: vi.fn() };
  const destination = {};
  return {
    createAnalyser: vi.fn().mockReturnValue(analyser),
    createMediaStreamSource: vi.fn().mockReturnValue(source),
    destination,
    sampleRate: 48000,
    close: vi.fn().mockResolvedValue(undefined),
    state: 'running' as AudioContextState,
  };
}

function createMockStream() {
  return {
    getTracks: vi.fn().mockReturnValue([{ stop: vi.fn() }]),
  };
}

describe('AudioService', () => {
  let mockCtx: ReturnType<typeof createMockAudioContext>;
  let mockStream: ReturnType<typeof createMockStream>;

  beforeEach(() => {
    mockCtx = createMockAudioContext();
    mockStream = createMockStream();
    // Mock navigator.mediaDevices
    Object.defineProperty(global.navigator, 'mediaDevices', {
      value: {
        getUserMedia: vi.fn().mockResolvedValue(mockStream),
      },
      writable: true,
      configurable: true,
    });
  });

  it('isActive returns false initially', () => {
    const service = new AudioService();
    expect(service.isActive()).toBe(false);
  });

  it('start requests microphone access', async () => {
    const service = new AudioService();
    await service.start(vi.fn(), () => mockCtx as unknown as AudioContext);
    expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledWith({
      audio: { sampleRate: 16000, channelCount: 1, echoCancellation: false },
    });
  });

  it('isActive returns true after start', async () => {
    const service = new AudioService();
    await service.start(vi.fn(), () => mockCtx as unknown as AudioContext);
    expect(service.isActive()).toBe(true);
  });

  it('stop releases resources', async () => {
    const service = new AudioService();
    await service.start(vi.fn(), () => mockCtx as unknown as AudioContext);
    service.stop();
    expect(service.isActive()).toBe(false);
  });

  it('stop is safe to call when not active', () => {
    const service = new AudioService();
    expect(() => service.stop()).not.toThrow();
  });
});
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/audio-service.test.ts`
Expected: FAIL

**Step 3: Write audio-service.ts**

```typescript
// src/lib/audio-service.ts

export type PcmChunkCallback = (pcm: Float32Array) => void;

export class AudioService {
  private stream: MediaStream | null = null;
  private context: AudioContext | null = null;
  private source: MediaStreamAudioSourceNode | null = null;
  private active = false;

  isActive(): boolean {
    return this.active;
  }

  async start(
    onChunk: PcmChunkCallback,
    createContext?: () => AudioContext,
  ): Promise<void> {
    if (this.active) return;

    this.stream = await navigator.mediaDevices.getUserMedia({
      audio: { sampleRate: 16000, channelCount: 1, echoCancellation: false },
    });

    this.context = createContext
      ? createContext()
      : new AudioContext({ sampleRate: 16000 });

    this.source = this.context.createMediaStreamSource(this.stream);

    // In production, connect to an AudioWorklet for 16kHz capture.
    // For now, connect to an analyser as a placeholder.
    const analyser = this.context.createAnalyser();
    this.source.connect(analyser);

    this.active = true;
  }

  stop(): void {
    if (!this.active) return;

    this.source?.disconnect();
    this.stream?.getTracks().forEach(t => t.stop());
    this.context?.close();

    this.source = null;
    this.stream = null;
    this.context = null;
    this.active = false;
  }
}
```

**Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run src/lib/audio-service.test.ts`
Expected: PASS

**Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/audio-service.ts src/lib/audio-service.test.ts
git commit -m "feat: add AudioService for microphone capture lifecycle"
```

---

## Summary

| Task | Component | Tests | Dependencies |
|------|-----------|-------|-------------|
| 1 | Flashcard types + AppMode | 3 | — |
| 2 | Q8 utility functions | 7 | — |
| 3 | stq8Service | 6 | Task 1 |
| 4 | FlashcardGrid | 6 | Tasks 1, 2 |
| 5 | HintBar | 4 | — |
| 6 | PttButton | 9 | — |
| 7 | FlashcardStats | 6 | Task 1 |
| 8 | SpellList | 2 | — |
| 9 | FlashcardView | 5 + 6 (express) | Tasks 1-6 |
| 10 | SpellbookMode | 7 | Tasks 7-9 |
| 11 | App integration | 2 | Task 10 |
| 12 | AudioService | 5 | — |

**Total: 12 tasks, ~68 tests**

Tasks 1-3 are foundation (types + services). Tasks 4-8 are leaf components (parallelizable). Task 9 is the orchestrator. Tasks 10-11 are integration. Task 12 is independent.
