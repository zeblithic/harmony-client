<script lang="ts">
  /**
   * ZEB-218 Sub-D Phase 1: the consumer-side library directory browser.
   * - Empty state CTA → opens `AddLibraryDialog`
   * - Library chips with remove ✕
   * - Aggregated catalog (deduped by community_id across libraries)
   * - Join button → `onJoin(invite_url)` callback (App wires to
   *   `redeem_invite` IPC, reusing ZEB-249's open-community invite path)
   *
   * Refreshes on the `library-directory-updated` IPC event with a 200ms
   * debounce so a burst of incoming entries collapses into one fetch.
   */
  import AddLibraryDialog from './AddLibraryDialog.svelte';
  import type { LibraryDirectoryService, LibraryInfo, DirectoryEntry } from '../library-directory-service';
  import type { TauriAdapter } from '../zenoh-service';

  let {
    service,
    adapter,
    onJoin,
    onClose,
  }: {
    service: LibraryDirectoryService;
    adapter: TauriAdapter;
    /** Called when the user clicks Join on an entry. Wired to redeem_invite. */
    onJoin: (inviteUrl: string) => Promise<void>;
    /** Closes the browser modal. */
    onClose: () => void;
  } = $props();

  let libraries: LibraryInfo[] = $state([]);
  let entries: DirectoryEntry[] = $state([]);
  let addDialogOpen = $state(false);
  let addPending = $state(false);
  let addError: string | null = $state(null);
  let removeError: string | null = $state(null);
  let joinPending = $state<string | null>(null); // community_id mid-join
  let joinError: string | null = $state(null);

  // `adapter.listen` returns `Promise<() => void>` (see TauriAdapter in
  // zenoh-service.ts). We stash the resolved unsubscribe so the $effect
  // cleanup can fire it without racing the in-flight Promise.
  let listenerUnsubscribe: (() => void) | null = null;
  let debounceTimer: ReturnType<typeof setTimeout> | null = null;

  async function refresh() {
    try {
      libraries = await service.list();
      entries = await service.browse();
    } catch (e) {
      // best-effort; UI just won't refresh
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('refresh failed:', msg);
    }
  }

  function scheduleRefresh() {
    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => {
      void refresh();
      debounceTimer = null;
    }, 200);
  }

  async function handleAddLibrary(addr: string) {
    addPending = true;
    addError = null;
    try {
      await service.add(addr);
      addDialogOpen = false;
      await refresh();
    } catch (e) {
      addError = e instanceof Error ? e.message : String(e);
    } finally {
      addPending = false;
    }
  }

  async function handleRemoveLibrary(addr: string) {
    let succeeded = false;
    try {
      await service.remove(addr);
      succeeded = true;
    } catch (e) {
      // R2 F4: surface the failure inline. Previously only console.warn'd,
      // so the user clicked ✕ and saw nothing happen.
      removeError = e instanceof Error ? e.message : String(e);
    } finally {
      // Always re-sync from backend regardless of outcome — defensive
      // UX in case the remove half-succeeded or the state diverged for
      // an unrelated reason.
      await refresh();
      // Clear stale error only after a successful refresh following a
      // successful remove (so subsequent unrelated re-renders don't
      // briefly hide the error message).
      if (succeeded) removeError = null;
    }
  }

  async function handleJoin(entry: DirectoryEntry) {
    joinPending = entry.community_id;
    joinError = null;
    try {
      await onJoin(entry.invite_url);
      onClose();
    } catch (e) {
      joinError = e instanceof Error ? e.message : String(e);
    } finally {
      joinPending = null;
    }
  }

  function shortAddr(hex: string): string {
    return hex.length >= 8 ? hex.slice(0, 8) + '…' : hex;
  }

  $effect(() => {
    void refresh();
    let cancelled = false;
    const pending = adapter.listen('library-directory-updated', () => {
      scheduleRefresh();
    });
    // `listen` may be Promise<() => void> (production tauri) or a
    // synchronous () => void (some test mocks). Branch on shape.
    if (pending && typeof (pending as Promise<unknown>).then === 'function') {
      (pending as Promise<() => void>).then((unsub) => {
        if (cancelled) {
          // Effect already torn down before listener attached — fire
          // immediately to avoid a leaked Tauri listener.
          try { unsub(); } catch { /* swallow */ }
        } else {
          listenerUnsubscribe = unsub;
        }
      });
    } else if (typeof pending === 'function') {
      listenerUnsubscribe = pending as () => void;
    }
    return () => {
      cancelled = true;
      if (listenerUnsubscribe) listenerUnsubscribe();
      if (debounceTimer) clearTimeout(debounceTimer);
    };
  });
</script>

<div class="browser" role="dialog" aria-modal="true" aria-labelledby="library-browser-title">
  <header class="header">
    <h2 id="library-browser-title">Browse communities</h2>
    <button type="button" class="close-btn" aria-label="Close" onclick={onClose}>✕</button>
  </header>

  {#if libraries.length === 0}
    <div class="empty">
      <p>Add a library to start browsing communities.</p>
      <button type="button" class="primary" onclick={() => (addDialogOpen = true)}>
        + Add a library
      </button>
    </div>
  {:else}
    <div class="libraries-bar">
      {#each libraries as lib (lib.address)}
        <span class="lib-chip" title={lib.address}>
          {shortAddr(lib.address)}
          <button
            type="button"
            class="lib-remove"
            aria-label={`Remove library ${shortAddr(lib.address)}`}
            onclick={() => handleRemoveLibrary(lib.address)}
          >✕</button>
        </span>
      {/each}
      <button type="button" class="add-lib-btn" onclick={() => (addDialogOpen = true)}>
        + Add library
      </button>
    </div>
    {#if removeError}
      <p class="remove-error" role="alert">Could not remove library: {removeError}</p>
    {/if}

    {#if entries.length === 0}
      <p class="empty-catalog">No communities listed yet.</p>
    {:else}
      <ul class="catalog">
        {#each entries as entry (entry.community_id)}
          <li class="catalog-row">
            <div class="row-main">
              <div class="row-title">{entry.name || '(unnamed)'}</div>
              <div class="row-desc">{entry.description}</div>
              {#if entry.topics.length > 0}
                <div class="topics">
                  {#each entry.topics as t}
                    <span class="topic-chip">{t}</span>
                  {/each}
                </div>
              {/if}
              <div class="row-meta">
                Listed by {entry.listed_by_count}
                {entry.listed_by_count === 1 ? 'library' : 'libraries'}
              </div>
            </div>
            <button
              type="button"
              class="join-btn"
              onclick={() => handleJoin(entry)}
              disabled={joinPending === entry.community_id}
            >
              {joinPending === entry.community_id ? 'Joining…' : 'Join'}
            </button>
          </li>
        {/each}
      </ul>
    {/if}

    {#if joinError}
      <p class="join-error">{joinError}</p>
    {/if}
  {/if}
</div>

{#if addDialogOpen}
  <div
    class="modal-overlay"
    role="presentation"
    onclick={() => (addDialogOpen = false)}
    onkeydown={(e) => { if (e.key === 'Escape') addDialogOpen = false; }}
  >
    <div
      class="modal-content"
      role="dialog"
      aria-modal="true"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => e.stopPropagation()}
    >
      <AddLibraryDialog
        onSubmit={handleAddLibrary}
        onCancel={() => { addDialogOpen = false; addError = null; }}
        pending={addPending}
        error={addError}
      />
    </div>
  </div>
{/if}

<style>
  .browser { padding: 16px; min-width: 480px; max-height: 80vh; overflow-y: auto; }
  .header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 12px; }
  .close-btn { background: none; border: none; font-size: 1.1rem; cursor: pointer; }
  .empty { text-align: center; padding: 24px; }
  .libraries-bar { display: flex; flex-wrap: wrap; gap: 6px; padding-bottom: 8px; border-bottom: 1px solid var(--border); margin-bottom: 12px; }
  .lib-chip { display: inline-flex; align-items: center; gap: 4px; padding: 2px 8px; background: rgba(120,140,200,0.2); border-radius: 12px; font-size: 0.75rem; font-family: monospace; }
  .lib-remove { background: none; border: none; cursor: pointer; padding: 0 2px; }
  .add-lib-btn { padding: 2px 8px; font-size: 0.75rem; }
  .empty-catalog { color: var(--text-secondary); padding: 16px; text-align: center; }
  .catalog { list-style: none; padding: 0; margin: 0; }
  .catalog-row { display: flex; justify-content: space-between; gap: 12px; padding: 12px 8px; border-bottom: 1px solid var(--border); }
  .row-title { font-weight: 600; }
  .row-desc { color: var(--text-secondary); font-size: 0.85rem; margin: 4px 0; }
  .topics { display: flex; flex-wrap: wrap; gap: 4px; margin-top: 4px; }
  .topic-chip { font-size: 0.7rem; padding: 1px 6px; background: rgba(120,140,200,0.15); border-radius: 8px; }
  .row-meta { font-size: 0.7rem; color: var(--text-secondary); margin-top: 4px; }
  .join-btn { align-self: center; padding: 6px 12px; background: rgba(120,140,200,0.4); border: none; border-radius: 4px; cursor: pointer; }
  .join-btn:disabled { opacity: 0.4; cursor: not-allowed; }
  .join-error { color: #d83c3e; font-size: 0.85rem; margin-top: 8px; }
  .remove-error { color: #d83c3e; font-size: 0.8rem; margin: 0 0 8px; }
  .modal-overlay { position: fixed; inset: 0; background: rgba(0,0,0,0.5); display: flex; align-items: center; justify-content: center; z-index: 1000; }
  .modal-content { background: var(--bg-secondary); border-radius: 6px; }
  .primary { background: rgba(120,140,200,0.4); padding: 8px 16px; border: none; border-radius: 4px; cursor: pointer; }
</style>
