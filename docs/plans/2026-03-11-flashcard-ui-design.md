# Flashcard UI Design (Spellbook Mode)

## Goal

A voice-driven flashcard practice interface within the Spellbook mode of
harmony-client. Users read Q8-BOX grids, speak the syllables via PTT, and
get immediate per-byte feedback. Supports an "express lane" mode where
consonant-only matches count as partial (yellow) passes.

Depends on the flashcard engine in stq8-core (PR #2 on harmony-stq8):
`format_box`, `format_flat`, `generate`, `validate_row`, `Level`.

## Spellbook Mode

New top-level mode alongside messages/vines/files in the nav.

**Two tabs:**
- **Spells** — bookmark list (Q8 page addresses into CAS). Empty state
  in v1, with a prompt to try Practice.
- **Practice** — the flashcard trainer.

**Toolbar controls:**
- Express Lane toggle (on/off, off by default)
- Level selector (Novice through Master, freely accessible)

**Layout:** Uses the existing 3-column grid. Nav has Spellbook icon.
Main panel has tab bar + content. Detail panel shows session stats.

## Q8-BOX Grid Display

Each byte is a 2x2 character cell (consonant row over vowel row) rendered
as a tight visual unit:
- **Zero internal spacing** between the 4 characters of a byte
- **Gap between bytes** (8px+) for visual grouping
- Characters use BOX set: consonants `A > < V`, vowels `O = X I`

Font: monospace, sized per level (large for Novice, scaled down for Master).

**Row highlighting states:**
- Upcoming: neutral
- Active: accent border around the row's byte cells
- Completed perfect: green tint
- Completed express: yellow tint (had at least one yellow byte)
- Mismatch: brief red flash on wrong bytes before row resets

**Hint toggle:** Shows Q8-FLAT phonetic text for the active row only,
below the grid (e.g. `KU'E 'O'I`).

## PTT Interaction

**Button:** Large circular button fixed at bottom-center of flashcard view.
States: idle (outline + mic icon), active (filled accent, glowing),
processing.

**Dual activation:**
- Mouse/touch: press and hold the button
- Keyboard: spacebar hold (when flashcard view focused)
- Spacebar programmatically activates the button for consistent visual
  feedback. Single reactive state (`pttActive`) drives both.

**Behavior:**
- Hold PTT = audio capture active, syllables classified and validated
- Release PTT = cancel current row (reset to row start, banked rows kept)
- 2-second momentum timeout = row resets (PTT stays held, retry immediately)
- Row completion = auto-advance to next row (or next card). Keep going
  while PTT held.

## Express Lane

When enabled, consonant-only matches count as partial passes.

**Per-byte evaluation (express OFF):**
- Correct syllable pair = Green (8 bits)
- Wrong = Red (0 bits, row fails)

**Per-byte evaluation (express ON):**
- Consonant + vowel correct = Green (8 bits)
- Consonant correct, vowel wrong = Yellow (4 bits, express match)
- Consonant wrong = Red (0 bits, row fails)

**Row outcome:** All green/yellow = pass. Any red = fail (row resets).

**Card outcome:**
- All bytes green = Perfect
- Any yellow bytes = Express (pass, not perfect)

Express lane is implemented as a validation-layer policy on top of the
existing 16-sound classifier — no core model changes needed. If the
classifier returns the right consonant but wrong vowel, that's a yellow.

## Session Stats

Displayed in the detail panel. Session-only, no persistence in v1.

| Stat | Description |
|------|-------------|
| Cards completed | Total passes this session |
| Perfect cards | All-green cards |
| Express cards | Cards with any yellow |
| Best time | Fastest card completion |
| Average time | Rolling mean |
| Previous time | Last card (immediate comparison) |
| Combo | Consecutive passes without PTT release or timeout |
| Effective bitrate | `credited_bits / elapsed_seconds` |

Bitrate: green bytes = 8 bits, yellow bytes = 4 bits. Sum credited bits
per completed card, divided by wall-clock seconds from first syllable
to last row pass.

## Component Architecture

**New components (`src/lib/components/`):**

| Component | Responsibility |
|-----------|---------------|
| `SpellbookMode.svelte` | Mode container. Tab bar, express toggle, level selector. |
| `SpellList.svelte` | Bookmark list (empty state v1). |
| `FlashcardView.svelte` | Practice view. Owns session state: card, row, per-byte results, timers, combo, stats. |
| `FlashcardGrid.svelte` | Pure display: Q8-BOX byte grid. Props: challenge, activeRow, rowResults. |
| `FlashcardStats.svelte` | Detail panel: all session stats. |
| `PttButton.svelte` | Fixed-bottom button. Spacebar binding. Events: pttstart/pttstop. |
| `HintBar.svelte` | Q8-FLAT display for active row. |

**New services:**

| Service | Responsibility |
|---------|---------------|
| `AudioService` | Web Audio context, mic permission, AudioWorklet capture at 16kHz. |
| `stq8Service` | WASM module loader, TypeScript wrapper for WasmPipeline methods. |

**Data flow:**
```
PttButton (spacebar/click)
  -> FlashcardView: pttActive = true
  -> AudioService.start()
  -> onChunk(pcm) -> stq8Service.process(pcm)
  -> UtteranceResult.syllables
  -> validate against current row (stq8Service.validateRow)
  -> per-byte green/yellow/red (express lane logic in FlashcardView)
  -> update FlashcardGrid display
  -> advance row or reset
```

## Testing Strategy

**Component tests (vitest + @testing-library/svelte):**
- FlashcardGrid renders correct BOX characters for challenge bytes
- FlashcardGrid highlights active row, shows green/yellow on completed rows
- PttButton fires pttstart/pttstop on mousedown/mouseup
- PttButton responds to spacebar (preventDefault on space to avoid scroll)
- HintBar shows Q8-FLAT for given row bytes
- FlashcardStats displays all stat values
- SpellbookMode tab switching (Spells <-> Practice)
- FlashcardView row advancement with mock stq8Service
- FlashcardView express lane scoring (consonant match -> yellow)
- FlashcardView PTT release resets current row, keeps banked rows

**Service tests (vitest, no DOM):**
- stq8Service wraps WASM methods correctly (mock WASM module)
- AudioService start/stop lifecycle (mock AudioContext)

**Not tested:** WASM internals (covered in stq8-core), actual mic capture
(hardware), visual pixel accuracy.
