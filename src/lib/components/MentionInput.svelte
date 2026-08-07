<script lang="ts">
  /**
   * ZEB-594 — contenteditable channel compose with atomic mention chips. The
   * contenteditable DOM is the source of truth; Svelte owns only the shell
   * (placeholder via CSS, the autocomplete dropdown). Picks splice a chip node at
   * the caret; serialize() reads the DOM. Retires the flat-text span-tracking
   * model (shiftTrackedSpans/reconcileCompose).
   */
  import { detectMentionTrigger, filterCandidates, serializeSegments } from '../mention-compose';
  import type { MentionCandidate } from '../mention-compose';
  import { domToSegments, createChip, chipToDeleteAt } from '../mention-dom';
  import MentionAutocomplete from './MentionAutocomplete.svelte';

  interface Props {
    candidates: MentionCandidate[];
    placeholder: string;
    ariaLabel: string;
    disabled: boolean;
    onSend: (payload: { body: string; mentions: string[] }) => void;
    onInput?: () => void;
  }
  const { candidates, placeholder, ariaLabel, disabled, onSend, onInput }: Props = $props();

  let editable: HTMLDivElement | undefined = $state();
  let composing = $state(false); // IME composition in flight
  let trigger = $state<{ query: string; atIndex: number } | null>(null);
  let acIndex = $state(0);
  const acCandidates = $derived(trigger ? filterCandidates(candidates, trigger.query) : []);
  const acOpen = $derived(acCandidates.length > 0);

  // Reset the dropdown when the roster changes (e.g. channel switch) so a stale
  // trigger from the previous channel can't linger. The draft content is NOT
  // cleared here — it is a preserved cross-channel draft.
  $effect(() => {
    void candidates;
    trigger = null;
    acIndex = 0;
  });

  export function serialize(): { body: string; mentions: string[] } {
    return serializeSegments(editable ? domToSegments(editable) : []);
  }
  export function clear(): void {
    if (editable) editable.replaceChildren();
    trigger = null;
    acIndex = 0;
  }
  export function focus(): void {
    editable?.focus();
  }

  /** The current selection if it is collapsed inside our editable, else null. */
  function caret(): { node: Node; offset: number } | null {
    const sel = window.getSelection();
    if (!sel || sel.rangeCount === 0 || !sel.isCollapsed) return null;
    const r = sel.getRangeAt(0);
    if (!editable || !editable.contains(r.startContainer)) return null;
    return { node: r.startContainer, offset: r.startOffset };
  }

  /** Re-detect the @-trigger from the live caret. Only fires outside IME. */
  function refreshTrigger() {
    if (composing) return;
    const c = caret();
    if (!c || c.node.nodeType !== Node.TEXT_NODE) {
      trigger = null;
      return;
    }
    const text = (c.node.textContent ?? '').slice(0, c.offset);
    trigger = detectMentionTrigger(text, c.offset);
    acIndex = 0;
  }

  function onInputEvent() {
    refreshTrigger();
    onInput?.();
  }

  /** Replace the active "@query" run with a chip node + trailing space. */
  function pick(candidate: MentionCandidate) {
    const c = caret();
    if (!editable || !trigger || !c || c.node.nodeType !== Node.TEXT_NODE) return;
    const textNode = c.node as Text;
    const range = document.createRange();
    range.setStart(textNode, trigger.atIndex); // the '@'
    range.setEnd(textNode, c.offset); // the caret
    range.deleteContents();
    const chip = createChip(document, candidate.ownerId, candidate.label);
    const space = document.createTextNode(' '); // nbsp keeps the boundary visible
    range.insertNode(space);
    range.insertNode(chip);
    // Caret after the space.
    const after = document.createRange();
    after.setStartAfter(space);
    after.collapse(true);
    const sel = window.getSelection();
    sel?.removeAllRanges();
    sel?.addRange(after);
    trigger = null;
    acIndex = 0;
    onInput?.();
  }

  function handleKeydown(e: KeyboardEvent) {
    // 1) Autocomplete hijack — runs before send.
    if (acOpen) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        acIndex = (acIndex + 1) % acCandidates.length;
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        acIndex = (acIndex - 1 + acCandidates.length) % acCandidates.length;
        return;
      }
      if (e.key === 'Enter' || e.key === 'Tab') {
        e.preventDefault();
        const candidate = acCandidates[acIndex];
        if (candidate) pick(candidate);
        else acIndex = 0;
        return;
      }
      if (e.key === 'Escape') {
        e.preventDefault();
        trigger = null;
        return;
      }
    }
    // 2) IME guard: never send while composing (also fixes the latent textarea bug).
    if (e.isComposing || e.keyCode === 229) return;
    // 3) Enter sends; Shift+Enter falls through to the browser (newline).
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      onSend(serialize());
      return;
    }
    // 4) Atomic chip delete at a boundary.
    if (e.key === 'Backspace' || e.key === 'Delete') {
      const c = caret();
      if (!c) return;
      const dir = e.key === 'Backspace' ? 'backward' : 'forward';
      const chip = chipToDeleteAt(c.node, c.offset, dir);
      if (chip) {
        e.preventDefault();
        chip.remove();
        refreshTrigger();
      }
    }
  }

  /** Paste plain text only (strip rich HTML); insert as a text node at the caret. */
  function handlePaste(e: ClipboardEvent) {
    e.preventDefault();
    const text = e.clipboardData?.getData('text/plain') ?? '';
    if (!text) return;
    const sel = window.getSelection();
    if (!sel || sel.rangeCount === 0) return;
    const range = sel.getRangeAt(0);
    range.deleteContents();
    const node = document.createTextNode(text);
    range.insertNode(node);
    range.setStartAfter(node);
    range.collapse(true);
    sel.removeAllRanges();
    sel.addRange(range);
    refreshTrigger();
    onInput?.();
  }
</script>

<div class="mention-input-wrap">
  <div
    bind:this={editable}
    class="mention-input"
    contenteditable={!disabled}
    role="textbox"
    tabindex={disabled ? -1 : 0}
    aria-multiline="true"
    aria-label={ariaLabel}
    aria-disabled={disabled}
    data-placeholder={placeholder}
    onkeydown={handleKeydown}
    oninput={onInputEvent}
    onkeyup={refreshTrigger}
    onclick={refreshTrigger}
    onpaste={handlePaste}
    oncompositionstart={() => (composing = true)}
    oncompositionend={() => {
      composing = false;
      refreshTrigger();
    }}
  ></div>
  {#if acOpen}
    <MentionAutocomplete candidates={acCandidates} activeIndex={acIndex} onPick={pick} />
  {/if}
</div>

<style>
  .mention-input-wrap {
    position: relative;
    flex: 1;
    min-width: 0;
  }
  .mention-input {
    min-height: 2.6rem;
    max-height: 12rem;
    overflow-y: auto;
    padding: 0.5rem 0.6rem;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--surface-raised);
    color: var(--text-primary);
    font: inherit;
    white-space: pre-wrap;
    word-break: break-word;
    outline: none;
  }
  .mention-input:focus {
    border-color: var(--accent);
  }
  .mention-input[aria-disabled='true'] {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .mention-input:empty::before {
    content: attr(data-placeholder);
    color: var(--text-muted);
    pointer-events: none;
  }
  /* Chip styling mirrors the read-side `.mention` accent chip. */
  .mention-input :global(.mention-chip) {
    display: inline;
    padding: 0.05rem 0.25rem;
    border-radius: 5px;
    background: color-mix(in srgb, var(--accent) 18%, transparent);
    color: var(--accent);
    white-space: nowrap;
  }
</style>
