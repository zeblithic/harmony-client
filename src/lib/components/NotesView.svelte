<script lang="ts">
  /**
   * ZEB-334 — Self-notes view.
   *
   * The always-present, private scratchpad shown as the default when no
   * community is joined (replacing the misleading "#general floating in the
   * void"). Reads/writes through NotesService only — no network plumbing —
   * so it works offline and across identity switches. Author label reuses the
   * configured display name (consistent with ZEB-337).
   */
  import type { NotesService, NoteEntry } from '../notes-service';

  let {
    notesService,
    ownerId,
    displayName,
  }: {
    notesService: NotesService;
    ownerId: string;
    displayName: string;
  } = $props();

  let entries = $state<NoteEntry[]>([]);
  let composeText = $state('');

  // Load on mount and reload whenever the active identity changes. Reading
  // ownerId here makes the effect re-run on identity switch, so notes never
  // bleed across identities.
  $effect(() => {
    entries = notesService.getEntries(ownerId);
  });

  const authorLabel = $derived(displayName?.trim() ? displayName : 'You');

  function submit() {
    const text = composeText.trim();
    if (!text) return;
    // Keep the text if it couldn't be saved (e.g. the owner id hasn't loaded
    // yet, so append is a no-op) — clearing it would silently lose the note.
    // (Cursor PR #180.)
    const entry = notesService.append(ownerId, text);
    if (!entry) return;
    entries = notesService.getEntries(ownerId);
    composeText = '';
  }

  function handleKeydown(e: KeyboardEvent) {
    // Enter sends; Shift+Enter inserts a newline (matches channel/DM compose).
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  }

  function formatTime(ts: number): string {
    return new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  }
</script>

<div class="notes-view" data-testid="notes-view">
  <header class="notes-header">
    <div class="notes-title"><span aria-hidden="true">📝</span> Notes</div>
    <p class="notes-subtitle">
      Private — only you can see this. Syncs across your devices in a future update.
    </p>
  </header>

  <div class="notes-stream" data-testid="notes-stream" role="log" aria-live="polite">
    {#if entries.length === 0}
      <p class="notes-empty">
        Nothing here yet. Jot down a thought, a link, or a to-do — it stays on this
        device, just for you.
      </p>
    {/if}
    {#each entries as entry (entry.id)}
      <article class="note-entry">
        <header class="note-meta">
          <span class="note-author">{authorLabel}</span>
          <time class="note-ts" datetime={new Date(entry.timestamp).toISOString()}>
            {formatTime(entry.timestamp)}
          </time>
        </header>
        <p class="note-body">{entry.text}</p>
      </article>
    {/each}
  </div>

  <div class="notes-compose">
    <textarea
      data-testid="notes-compose"
      bind:value={composeText}
      onkeydown={handleKeydown}
      class="notes-compose-input"
      placeholder="Write a note to yourself…"
      rows="2"
      aria-label="Write a note to yourself"
    ></textarea>
  </div>
</div>

<style>
  .notes-view {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
    height: 100%;
  }
  .notes-header {
    padding: 10px 16px;
    border-bottom: 1px solid var(--border);
  }
  .notes-title {
    color: var(--text-primary);
    font-weight: 600;
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .notes-subtitle {
    margin: 2px 0 0;
    color: var(--text-secondary);
    font-size: 0.78rem;
  }
  .notes-stream {
    flex: 1;
    overflow-y: auto;
    padding: 12px 0;
  }
  .notes-empty {
    color: var(--text-secondary);
    font-size: 0.9rem;
    text-align: center;
    max-width: 380px;
    margin: 40px auto;
    line-height: 1.5;
  }
  .note-entry {
    padding: 6px 16px;
  }
  .note-entry:hover {
    background: var(--bg-tertiary);
  }
  .note-meta {
    display: flex;
    gap: 8px;
    align-items: baseline;
  }
  .note-author {
    color: var(--text-primary);
    font-weight: 500;
    font-size: 0.9rem;
  }
  .note-ts {
    color: var(--text-secondary);
    font-size: 0.7rem;
  }
  .note-body {
    margin: 2px 0 0;
    color: var(--text-primary);
    white-space: pre-wrap;
    word-wrap: break-word;
  }
  .notes-compose {
    border-top: 1px solid var(--border);
    padding: 8px 16px 12px;
  }
  .notes-compose-input {
    width: 100%;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    padding: 8px 10px;
    font-size: 0.9rem;
    font-family: inherit;
    resize: vertical;
    box-sizing: border-box;
  }
  .notes-compose-input:focus {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
  }
</style>
