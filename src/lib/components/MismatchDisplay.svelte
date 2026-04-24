<script lang="ts">
  import { formatFlatBytes, formatFlatNibbles } from '../q8-flat';

  let {
    expectedBytes,
    heardNibbles,
    firstDiffNibbleIdx,
  }: {
    expectedBytes: number[];
    heardNibbles: number[];
    /** Nibble index where heard first diverges from expected. */
    firstDiffNibbleIdx: number;
  } = $props();

  // Label column width — "Expected: " and "Heard:    " are both 10 chars.
  // Kept in sync with the literal prefixes in the <pre> below.
  const LABEL_WIDTH = 10;

  let expectedLine = $derived(formatFlatBytes(expectedBytes));
  let heardLine = $derived(formatFlatNibbles(heardNibbles));

  // Caret column: each Q8-FLAT word is 4 chars + 1 trailing space (byteIdx * 5),
  // each nibble within a word is 2 chars (syllableIdx * 2). Offset by label width.
  // Null when there's no diff — the caret row is omitted entirely in that case
  // so a perfect match can never render a misleading ^^ at column 0.
  let caretPrefix = $derived.by(() => {
    if (firstDiffNibbleIdx < 0) return null;
    const byteIdx = Math.floor(firstDiffNibbleIdx / 2);
    const syllableIdx = firstDiffNibbleIdx % 2;
    return ' '.repeat(LABEL_WIDTH + byteIdx * 5 + syllableIdx * 2);
  });
</script>

<div class="mismatch" role="status" aria-live="polite" data-testid="mismatch-display">
  {#if caretPrefix !== null}
    <pre aria-label="Mismatch feedback">Expected: {expectedLine}
Heard:    {heardLine}
{caretPrefix}^^</pre>
  {:else}
    <pre aria-label="Mismatch feedback">Expected: {expectedLine}
Heard:    {heardLine}</pre>
  {/if}
</div>

<style>
  .mismatch {
    font-family: 'Courier New', Courier, monospace;
    color: var(--text-warning, #f0b232);
    padding: 8px 12px;
    background: rgba(240, 178, 50, 0.08);
    border-radius: 4px;
    max-width: fit-content;
    margin: 0 auto;
    font-size: 0.9rem;
  }

  pre {
    margin: 0;
    white-space: pre;
    line-height: 1.25;
  }
</style>
