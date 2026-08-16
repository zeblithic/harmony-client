<script lang="ts">
  /**
   * ZEB-334 / ZEB-417 — Self-notes view.
   *
   * The always-present, private scratchpad shown as the default when no
   * community is joined (replacing the misleading "#general floating in the
   * void"). Reads/writes through NotesService only — IPC-backed since ZEB-417,
   * synced across the owner's devices. Author label reuses the configured
   * display name (consistent with ZEB-337).
   */
  import { onMount, onDestroy } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { formatMessageTimestamp, formatFullTimestamp, type TimeFormatPrefs } from '../time-format';
  import { dayClock } from '../day-clock';
  import { timeFormatPrefs } from '../time-format-service';
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
  // bleed across identities. `load` hydrates the SERVICE state (and fires
  // onChange) so `append`/`notes-changed` updates all flow through a single
  // source of truth (notesService.entries) — no direct getEntries reads that
  // could diverge from it.
  //
  // The `active` flag guards against a stale-ownerId race: if ownerId changes
  // mid-load, the in-flight `load`'s onChange must not clobber `entries` with
  // the prior owner's notes. Cleanup flips `active` false so only the latest
  // effect run mirrors into `entries`.
  $effect(() => {
    let active = true;
    const owner = ownerId;
    notesService.onChange = () => {
      if (active) entries = [...notesService.entries];
    };
    void notesService.load(owner);
    return () => {
      active = false;
    };
  });

  // ZEB-417: subscribe to backend sync events so inbound changes are reflected
  // immediately without a manual reload. Guard the reload against the current
  // ownerId so a late event for a switched-away identity can't repopulate.
  // Cleaned up on component destroy.
  let unlistenNotesChanged: (() => void) | null = null;
  onMount(async () => {
    unlistenNotesChanged = await listen('notes-changed', () => {
      void notesService.load(ownerId);
    });
  });
  onDestroy(() => {
    unlistenNotesChanged?.();
  });

  const authorLabel = $derived(displayName?.trim() ? displayName : 'You');

  async function submit() {
    const text = composeText.trim();
    if (!text) return;
    // Keep the text if it couldn't be saved (e.g. the owner id hasn't loaded
    // yet, so append is a no-op) — clearing it would silently lose the note.
    // (Cursor PR #180.)
    const entry = await notesService.append(ownerId, text);
    if (!entry) return;
    // `append` already updated notesService.entries and fired onChange (which
    // mirrors into `entries`), and inbound sync arrives via notes-changed — so
    // a re-fetch here is redundant and risky: a failed getEntries returning []
    // would hide the note we just saved. Trust the service state.
    composeText = '';
  }

  function handleKeydown(e: KeyboardEvent) {
    // Enter sends; Shift+Enter inserts a newline (matches channel/DM compose).
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      void submit();
    }
  }

  // ZEB-943: format against the app-wide day clock so labels reclassify at
  // local midnight without a remount (day-clock.ts). No per-message timer.
  // `now` comes from the template call site (`$dayClock`) so Svelte tracks the
  // store as a render dependency.
  function formatTime(ts: number, now: number, prefs: TimeFormatPrefs): string {
    return formatMessageTimestamp(ts, now, prefs);
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
          <time class="note-ts" datetime={new Date(entry.timestamp).toISOString()} title={formatFullTimestamp(entry.timestamp, $timeFormatPrefs)}>
            {formatTime(entry.timestamp, $dayClock, $timeFormatPrefs)}
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
