<script lang="ts">
  /**
   * ZEB-218 Sub-D Phase 1: the consumer-side library directory browser.
   * - Empty state CTA → opens `AddLibraryDialog`
   * - Library chips with remove ✕
   * - Aggregated catalog (deduped by community_id across libraries)
   * - Join button → `onJoin(community_id)` callback (App wires to
   *   `join_open_community` IPC — ZEB-252 Sub-D Phase 6. The backend
   *   re-resolves the matching entry server-side and delegates to the
   *   existing `redeem_invite_inner` codepath, so end state is identical
   *   to what `redeem_invite(invite_url)` produces.)
   *
   * Refreshes on the `library-directory-updated` IPC event with a 200ms
   * debounce so a burst of incoming entries collapses into one fetch.
   */
  import AddLibraryDialog from './AddLibraryDialog.svelte';
  import type {
    LibraryDirectoryService,
    LibraryInfo,
    DirectoryEntry,
    DiscoveredLibraryInfo,
  } from '../library-directory-service';
  import type { TauriAdapter } from '../zenoh-service';
  import type { RedeemInviteResultDto } from '../community-service';

  let {
    service,
    adapter,
    onJoin,
    onOpenJoinIroh,
    onJoinedUi,
    onClose,
  }: {
    service: LibraryDirectoryService;
    adapter: TauriAdapter;
    /** Called when the user clicks Join on an entry. Wired to join_open_community.
     *  Receives the entry's `community_id` (32-hex-char SpaceId) so the backend
     *  can re-resolve the directory entry server-side at click time. This
     *  PERFORMS A BACKEND JOIN — used for the legacy/local LAN redeem path. */
    onJoin: (communityId: string) => Promise<void>;
    /**
     * Open-community cross-WAN first-contact join. When provided, Join routes
     * through this (the entry's `invite_url`) instead of the LAN `onJoin`
     * path, so the returned `RedeemInviteResultDto.status` can drive a
     * cold-start "searching" / "rejected" banner. The open-join backend ALREADY
     * commits membership, so on `"joined"` the component runs only the UI
     * follow-up (`onJoinedUi`, if provided) and closes — it does NOT re-invoke
     * `onJoin`, which would trigger a redundant SECOND (LAN) backend join. Wired
     * to `connectivity_open_join_iroh(inviteUrl)`. Omit to keep the pure LAN
     * `onJoin` behavior (existing callers / tests).
     */
    onOpenJoinIroh?: (inviteUrl: string) => Promise<RedeemInviteResultDto>;
    /**
     * UI-only follow-up after a successful cross-WAN open-join (`"joined"`).
     * Runs the App-side nav-update / community-switch / member-refresh WITHOUT
     * performing a backend join (the open-join IPC already committed
     * membership). Receives the open-join result DTO (carrying the committed
     * `communityId` + `communityName`). Called only on the `onOpenJoinIroh`
     * `"joined"` path; absent → the component just closes the modal. The
     * status-absent legacy result still falls through to the backend `onJoin`.
     */
    onJoinedUi?: (result: RedeemInviteResultDto) => Promise<void> | void;
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
  // Open-community cross-WAN cold-start state (ZEB first-contact). Keyed by
  // community_id so the banner attaches to the row the user clicked.
  //   'searching' → retryable: no beacon reachable yet, the node keeps
  //                 retrying on its transport-epoch re-arm (non-blocking).
  //   'rejected'  → the beacon explicitly declined the join (banned / bad
  //                 capability) — a distinct blocked state, NOT the spinner.
  let coldStartSearching = $state<string | null>(null);
  let coldStartRejected = $state<string | null>(null);

  // ZEB-279 Sub-D Phase 2: discovered libraries (auto-announce ingest).
  // Filtered IPC excludes already-added libraries, so a row migrates
  // from `discovered` to `libraries` on the next refetch after `add`.
  let discovered: DiscoveredLibraryInfo[] = $state([]);
  let discoveredOpen: boolean = $state(false);
  // F5 (CodeAnt + CodeRabbit): once the user touches the toggle, auto-
  // expand on refresh stops firing — otherwise every refresh with N>0
  // would re-open a panel the user just collapsed. The flag also flips
  // on the FIRST auto-expand so subsequent N=0 → N>0 transitions don't
  // re-trigger either (the auto-expand fires exactly once over the
  // component's lifetime).
  let discoveredManuallyToggled: boolean = $state(false);
  let addingDiscovered: Record<string, boolean> = $state({});
  let discoveredError: Record<string, string> = $state({});

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
    // ZEB-279 Sub-D Phase 2: fetch discovered list independently so a
    // Phase 1 refresh failure doesn't blank the discovered panel and
    // vice-versa.
    try {
      discovered = await service.listDiscovered();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('listDiscovered failed:', msg);
      discovered = [];
    }
    // Auto-expand the section the first time we observe entries.
    // F5: only fire if the user has NEVER touched the toggle. Manual
    // collapse is sticky — re-opening a panel the user just dismissed
    // is hostile.
    if (discovered.length > 0 && !discoveredOpen && !discoveredManuallyToggled) {
      discoveredOpen = true;
      discoveredManuallyToggled = true;
    }
  }

  function toggleDiscovered() {
    discoveredOpen = !discoveredOpen;
    discoveredManuallyToggled = true;
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

  async function handleAddDiscovered(libraryAddr: string) {
    addingDiscovered[libraryAddr] = true;
    discoveredError[libraryAddr] = '';
    let succeeded = false;
    try {
      await service.add(libraryAddr);
      succeeded = true;
      // Refetch is also triggered by the `library-directory-updated`
      // event listener, but call explicitly here as belt-and-suspenders
      // for cases where the event hasn't propagated yet (or to flush
      // the row from the discovered list immediately on success).
      await refresh();
    } catch (e) {
      discoveredError[libraryAddr] = e instanceof Error ? e.message : String(e);
    } finally {
      // F6 (CodeAnt Minor): prune per-item state maps rather than
      // setting `false` / empty string. Across long sessions where
      // many discovered rows are added, unbounded growth here is a
      // small but real leak.
      delete addingDiscovered[libraryAddr];
      if (succeeded) {
        delete discoveredError[libraryAddr];
      }
    }
  }

  async function handleJoin(entry: DirectoryEntry) {
    joinPending = entry.community_id;
    joinError = null;
    // Clear any prior cold-start banner for this row before re-attempting.
    coldStartSearching = null;
    coldStartRejected = null;
    try {
      // Open-community cross-WAN first-contact path: when wired, route the
      // entry's invite URL through `connectivity_open_join_iroh` so its
      // `status` can drive a cold-start banner. The directory only lists OPEN
      // communities, so every entry is eligible for this path.
      if (onOpenJoinIroh) {
        const result = await onOpenJoinIroh(entry.invite_url);
        if (result.status === 'searching') {
          // RETRYABLE cold start — keep the modal open and show a
          // non-blocking "still searching" banner. NOT an error/failure.
          coldStartSearching = entry.community_id;
          return;
        }
        if (result.status === 'rejected') {
          // Distinct blocked state — the community declined the join (or an
          // internal dial invariant failed). NOT a retryable spinner.
          coldStartRejected = entry.community_id;
          return;
        }
        if (result.status === 'joined') {
          // The open-join backend ALREADY committed membership. Run only the
          // UI follow-up (nav-update / switch / member-refresh) and close — do
          // NOT call the legacy `onJoin`, which would trigger a redundant
          // SECOND backend (LAN) join.
          await onJoinedUi?.(result);
          onClose();
          return;
        }
        // status absent (legacy/local result) → fall through to the normal
        // backend joined success path below.
      }
      await onJoin(entry.community_id);
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

  <!-- ZEB-279 R2 F1: discovered-section template extracted to a single
       {#snippet} so both the empty-libraries-CTA branch and the populated
       branch render the same markup. Future updates touch one place. -->
  {#snippet discoveredPanel()}
    <section class="discovered-section">
      <button
        type="button"
        class="discovered-toggle"
        onclick={toggleDiscovered}
        aria-expanded={discoveredOpen}
      >
        {discoveredOpen ? '▼' : '▶'} Discovered libraries ({discovered.length})
      </button>
      {#if discoveredOpen}
        <ul class="discovered-list">
          {#each discovered as d (d.libraryAddr)}
            <li class="discovered-row">
              <div class="discovered-meta">
                <strong>{d.name}</strong>
                <span class="discovered-desc">{d.description}</span>
                <span class="discovered-addr">({shortAddr(d.libraryAddr)})</span>
              </div>
              <button
                type="button"
                class="discovered-add"
                aria-label={d.name
                  ? `Add ${d.name} (${shortAddr(d.libraryAddr)})`
                  : `Add library ${shortAddr(d.libraryAddr)}`}
                onclick={() => handleAddDiscovered(d.libraryAddr)}
                disabled={addingDiscovered[d.libraryAddr]}
              >
                {addingDiscovered[d.libraryAddr] ? 'Adding…' : 'Add'}
              </button>
              {#if discoveredError[d.libraryAddr]}
                <span class="discovered-error" role="alert">{discoveredError[d.libraryAddr]}</span>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>
  {/snippet}

  {#if libraries.length === 0}
    <div class="empty">
      <p>Add a library to start browsing communities.</p>
      <button type="button" class="primary" onclick={() => (addDialogOpen = true)}>
        + Add a library
      </button>
    </div>
    <!-- ZEB-279: discovered panel may surface entries even before the
         user has added any library — show it in the empty state too. -->
    {@render discoveredPanel()}
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

    <!-- ZEB-279 Sub-D Phase 2: discovered libraries section.
         Click-to-toggle; auto-expanded by refresh() when N>0. -->
    {@render discoveredPanel()}

    {#if entries.length === 0}
      <p class="empty-catalog">No communities listed yet.</p>
    {:else}
      <ul class="catalog">
        {#each entries as entry (entry.community_id)}
          <li class="catalog-row">
            <div class="row-main">
              <div class="row-title">
                <span class="community-name">{entry.name || '(unnamed)'}</span>
                {#if entry.unattested}
                  <span
                    class="unattested-badge"
                    role="img"
                    aria-label="One or more libraries' wrapping signatures failed to verify for this entry"
                    title="Unattested: at least one broadcasting library's signature failed to verify. The community admin's signature is still valid; the listing's content is trustworthy."
                  >
                    ⚠ Unattested
                  </span>
                {/if}
              </div>
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
            {#if coldStartSearching === entry.community_id}
              <!-- Non-blocking, retryable cold-start: no beacon reachable yet.
                   The node keeps retrying on its transport-epoch re-arm — this
                   is NOT a failure, so style it as an in-progress notice (NOT
                   the red error banner). -->
              <div class="searching-banner" data-testid="cold-start-searching" role="status">
                <div class="spinner" aria-hidden="true"></div>
                <span>
                  Searching for the community — no one's reachable yet, we'll keep trying.
                </span>
              </div>
            {/if}
            {#if coldStartRejected === entry.community_id}
              <!-- Distinct blocked state: the community explicitly declined the
                   join (banned / bad capability). NOT the searching spinner. -->
              <div class="rejected-banner" data-testid="cold-start-rejected" role="alert">
                This community declined the join request.
              </div>
            {/if}
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
      tabindex="-1"
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
  .lib-chip { display: inline-flex; align-items: center; gap: 4px; padding: 2px 8px; background: color-mix(in srgb, var(--library-accent) 20%, transparent); border-radius: 12px; font-size: 0.75rem; font-family: var(--font-mono); }
  .lib-remove { background: none; border: none; cursor: pointer; padding: 0 2px; }
  .add-lib-btn { padding: 2px 8px; font-size: 0.75rem; }
  .empty-catalog { color: var(--text-secondary); padding: 16px; text-align: center; }
  .catalog { list-style: none; padding: 0; margin: 0; }
  .catalog-row { display: flex; justify-content: space-between; gap: 12px; padding: 12px 8px; border-bottom: 1px solid var(--border); }
  .row-title { font-weight: 600; }
  .row-desc { color: var(--text-secondary); font-size: 0.85rem; margin: 4px 0; }
  .topics { display: flex; flex-wrap: wrap; gap: 4px; margin-top: 4px; }
  .topic-chip { font-size: 0.7rem; padding: 1px 6px; background: color-mix(in srgb, var(--library-accent) 15%, transparent); border-radius: 8px; }
  .row-meta { font-size: 0.7rem; color: var(--text-secondary); margin-top: 4px; }
  .join-btn { align-self: center; padding: 6px 12px; background: color-mix(in srgb, var(--library-accent) 40%, transparent); border: none; border-radius: 4px; cursor: pointer; }
  .join-btn:disabled { opacity: 0.4; cursor: not-allowed; }
  .join-error { color: var(--danger-muted); font-size: 0.85rem; margin-top: 8px; }
  /* Cold-start "still searching" — non-blocking, in-progress (NOT an error).
     Neutral/secondary styling with the same spinner the redeem flow uses. */
  .searching-banner {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    margin-top: 8px;
    padding: 8px 10px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-secondary);
    font-size: 0.8rem;
  }
  .spinner {
    width: 12px;
    height: 12px;
    flex: 0 0 auto;
    border: 2px solid var(--accent);
    border-top-color: transparent;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to { transform: rotate(360deg); }
  }
  /* Cold-start "rejected" — distinct blocked state (red, like join-error). */
  .rejected-banner {
    width: 100%;
    margin-top: 8px;
    padding: 8px 10px;
    background: var(--bg-tertiary);
    border: 1px solid var(--danger-muted);
    border-radius: 4px;
    color: var(--danger-muted);
    font-size: 0.8rem;
  }
  .remove-error { color: var(--danger-muted); font-size: 0.8rem; margin: 0 0 8px; }
  .modal-overlay { position: fixed; inset: 0; background: var(--overlay); display: flex; align-items: center; justify-content: center; z-index: 1000; }
  .modal-content { background: var(--bg-secondary); border-radius: 6px; }
  .primary { background: color-mix(in srgb, var(--library-accent) 40%, transparent); padding: 8px 16px; border: none; border-radius: 4px; cursor: pointer; }

  /* ZEB-279 Sub-D Phase 2: discovered libraries panel. */
  .discovered-section { margin: 0.5rem 0 0.75rem; }
  .discovered-toggle {
    background: transparent;
    border: none;
    cursor: pointer;
    font-weight: 600;
    padding: 0.25rem 0;
    width: 100%;
    text-align: left;
    font-size: 0.9rem;
    color: inherit;
  }
  .discovered-list { list-style: none; padding: 0; margin: 0.25rem 0 0; }
  .discovered-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 6px 8px;
    border-bottom: 1px solid var(--border);
    flex-wrap: wrap;
  }
  .discovered-meta {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .discovered-desc {
    font-size: 0.85rem;
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 60ch;
  }
  .discovered-addr {
    font-size: 0.75rem;
    color: var(--text-secondary);
    font-family: var(--font-mono);
  }
  .discovered-add {
    padding: 4px 10px;
    background: color-mix(in srgb, var(--library-accent) 40%, transparent);
    border: none;
    border-radius: 4px;
    cursor: pointer;
  }
  .discovered-add:disabled { opacity: 0.4; cursor: not-allowed; }
  .discovered-error {
    color: var(--danger-muted);
    font-size: 0.8rem;
    width: 100%;
  }

  /* ZEB-280 Sub-D Phase 3: inline unattested badge. Amber (NOT red)
     because the admin sig is still the trust anchor for content; only
     the wrapping (transport-integrity) attestation is compromised. */
  .unattested-badge {
    display: inline-block;
    margin-left: 0.5rem;
    padding: 0.125rem 0.5rem;
    font-size: 0.75rem;
    font-weight: 500;
    background-color: #fef3c7; /* amber-100 */
    color: #92400e; /* amber-800 */
    border: 1px solid #fcd34d; /* amber-300 */
    border-radius: 0.25rem;
    vertical-align: middle;
    cursor: help; /* hover tooltip cue */
  }
</style>
