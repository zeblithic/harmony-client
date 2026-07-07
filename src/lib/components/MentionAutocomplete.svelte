<script lang="ts">
  /**
   * ZEB-588 — purely presentational @-mention dropdown. The parent owns all
   * state: it calls `filterCandidates`, owns `activeIndex`, and handles
   * ↑/↓/Enter/Esc on the shared compose textarea (so keys are intercepted on the
   * element that actually has focus). This component only renders the given
   * (already-filtered) candidates and reports a click via `onPick`.
   *
   * Rows use `onmousedown` + `preventDefault` (not `onclick`) so picking does not
   * blur the textarea / collapse its selection before the pick is applied.
   */
  import type { MentionCandidate } from '../mention-compose';

  interface Props {
    candidates: MentionCandidate[];
    activeIndex: number;
    onPick: (c: MentionCandidate) => void;
  }
  const { candidates, activeIndex, onPick }: Props = $props();
</script>

{#if candidates.length > 0}
  <ul class="mention-autocomplete" role="listbox" data-testid="mention-autocomplete">
    {#each candidates as c, i (c.ownerId)}
      <li
        class="option"
        class:active={i === activeIndex}
        role="option"
        aria-selected={i === activeIndex}
        data-testid="mention-option"
        data-active={i === activeIndex ? 'true' : undefined}
      >
        <button
          type="button"
          tabindex="-1"
          onmousedown={(e) => {
            e.preventDefault();
            onPick(c);
          }}
        >
          {c.label}
        </button>
      </li>
    {/each}
  </ul>
{/if}

<style>
  .mention-autocomplete {
    position: absolute;
    bottom: 100%;
    left: 0;
    margin: 0 0 0.25rem;
    padding: 0.25rem;
    list-style: none;
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: var(--shadow-e2);
    max-height: 12rem;
    overflow-y: auto;
    z-index: 50;
    min-width: 12rem;
  }
  .option button {
    display: block;
    width: 100%;
    text-align: left;
    padding: 0.3rem 0.5rem;
    background: transparent;
    border: none;
    color: var(--text-primary);
    border-radius: 5px;
    cursor: pointer;
    font: inherit;
  }
  .option.active button,
  .option button:hover {
    background: var(--accent);
  }
</style>
