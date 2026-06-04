<script lang="ts">
  /**
   * ZEB-370 Phase 1: minimal Friends settings sub-panel.
   *
   * - Lists active friends (display + short owner_id) via `list_friends`.
   * - "Generate friend link" mints a `harmony://friend/...` URL to copy.
   * - "Add friend" redeems a pasted URL.
   * - Per-row "Unfriend" writes a Revoked tombstone.
   *
   * Backed by `FriendService` (passed in so it shares the app's single adapter
   * + event wiring); re-fetches the list on the `friend-list-changed` event.
   * Mirrors `NetworkDiscoverabilitySettings.svelte`'s self-contained,
   * runes-based settings-panel shape.
   */
  import { onMount, onDestroy } from 'svelte';
  import type { FriendService, FriendDto } from '../friend-service';

  let { service }: { service: FriendService } = $props();

  let friends = $state<FriendDto[]>([]);
  let loading = $state(true);
  let error = $state<string | null>(null);

  // Generate-link state.
  let generatedUrl = $state<string | null>(null);
  let generating = $state(false);

  // Add-friend state.
  let pasteUrl = $state('');
  let redeeming = $state(false);
  let addStatus = $state<string | null>(null);

  // Per-row in-flight unfriend guard (by owner_id hex).
  let unfriending = $state<Set<string>>(new Set());

  // Unsubscribe handle for our `friend-list-changed` listener (set in onMount).
  let unsubscribeChanged: (() => void) | null = null;

  async function refresh(): Promise<void> {
    try {
      friends = await service.listFriends();
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    // Re-fetch whenever the backend signals a change (redeem / accept /
    // unfriend, possibly on another device). Register our OWN listener and
    // keep the returned unsubscribe so unmounting only removes ours — a
    // second concurrently-mounted panel keeps its own subscription.
    unsubscribeChanged = service.onFriendsChanged(() => {
      void refresh();
    });
    void refresh();
  });

  onDestroy(() => {
    // Remove only this panel's listener (no shared-slot to null out).
    unsubscribeChanged?.();
    unsubscribeChanged = null;
  });

  async function handleGenerate(): Promise<void> {
    if (generating) return;
    generating = true;
    try {
      generatedUrl = await service.generateFriendToken();
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      generating = false;
    }
  }

  async function handleCopy(): Promise<void> {
    if (!generatedUrl) return;
    try {
      await navigator.clipboard.writeText(generatedUrl);
    } catch {
      // Clipboard may be unavailable (headless / permission); the URL stays
      // visible in the readonly input for manual copy.
    }
  }

  async function handleAdd(): Promise<void> {
    const url = pasteUrl.trim();
    if (redeeming || url.length === 0) return;
    redeeming = true;
    addStatus = null;
    try {
      const result = await service.redeemFriendToken(url);
      addStatus = `Added ${result.display ?? shortId(result.ownerIdHex)}`;
      pasteUrl = '';
      await refresh();
    } catch (e) {
      addStatus = `Failed: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      redeeming = false;
    }
  }

  async function handleUnfriend(ownerIdHex: string): Promise<void> {
    if (unfriending.has(ownerIdHex)) return;
    unfriending = new Set(unfriending).add(ownerIdHex);
    try {
      await service.unfriend(ownerIdHex);
      await refresh();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      const next = new Set(unfriending);
      next.delete(ownerIdHex);
      unfriending = next;
    }
  }

  function shortId(hex: string): string {
    return hex.length > 12 ? `${hex.slice(0, 12)}…` : hex;
  }
</script>

<div class="friends-section" data-testid="friends-panel">
  <div class="section-header">
    <h4 class="section-title">Friends</h4>
  </div>

  {#if error}
    <p class="error-text" data-testid="friends-error">{error}</p>
  {/if}

  <!-- Active friends list. -->
  {#if loading}
    <p class="muted">Loading…</p>
  {:else if friends.length === 0}
    <p class="muted" data-testid="friends-empty">No friends yet. Share a friend link to connect.</p>
  {:else}
    <ul class="friend-list" data-testid="friend-list">
      {#each friends as f (f.ownerIdHex)}
        <li class="friend-row">
          <div class="friend-id">
            <span class="friend-name">{f.display ?? shortId(f.ownerIdHex)}</span>
            <span class="friend-addr" title={f.ownerIdHex}>{shortId(f.ownerIdHex)}</span>
          </div>
          <button
            type="button"
            class="unfriend-btn"
            disabled={unfriending.has(f.ownerIdHex)}
            onclick={() => handleUnfriend(f.ownerIdHex)}
            data-testid="unfriend-btn"
          >
            {unfriending.has(f.ownerIdHex) ? '…' : 'Unfriend'}
          </button>
        </li>
      {/each}
    </ul>
  {/if}

  <!-- Generate a friend link to share. -->
  <div class="action-block">
    <button
      type="button"
      class="primary-btn"
      disabled={generating}
      onclick={handleGenerate}
      data-testid="generate-friend-link"
    >
      {generating ? 'Generating…' : 'Generate friend link'}
    </button>
    {#if generatedUrl}
      <div class="generated-row">
        <input
          type="text"
          readonly
          class="url-input"
          value={generatedUrl}
          data-testid="generated-url"
          onfocus={(e) => (e.currentTarget as HTMLInputElement).select()}
        />
        <button type="button" class="secondary-btn" onclick={handleCopy} data-testid="copy-url">
          Copy
        </button>
      </div>
    {/if}
  </div>

  <!-- Add a friend by pasting their link. -->
  <div class="action-block">
    <label class="add-label" for="friend-url-input">Add friend</label>
    <div class="add-row">
      <input
        id="friend-url-input"
        type="text"
        class="url-input"
        placeholder="harmony://friend/…"
        bind:value={pasteUrl}
        data-testid="add-friend-input"
      />
      <button
        type="button"
        class="primary-btn"
        disabled={redeeming || pasteUrl.trim().length === 0}
        onclick={handleAdd}
        data-testid="add-friend-btn"
      >
        {redeeming ? 'Adding…' : 'Add'}
      </button>
    </div>
    {#if addStatus}
      <p class="muted" data-testid="add-status">{addStatus}</p>
    {/if}
  </div>
</div>

<style>
  .friends-section {
    padding: 12px 0;
  }

  .section-header {
    margin-bottom: 8px;
  }

  .section-title {
    margin: 0;
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .friend-list {
    list-style: none;
    margin: 0 0 12px;
    padding: 0;
  }

  .friend-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 6px 0;
    border-bottom: 1px solid var(--border, #2a2a2a);
  }

  .friend-id {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }

  .friend-name {
    font-size: 13px;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .friend-addr {
    font-size: 11px;
    color: var(--text-secondary);
    font-family: var(--font-mono, monospace);
  }

  .unfriend-btn {
    flex-shrink: 0;
    font-size: 12px;
    padding: 4px 10px;
    border-radius: 4px;
    border: 1px solid var(--border, #555);
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
  }

  .unfriend-btn:hover:not(:disabled) {
    border-color: #d83c3e;
    color: #d83c3e;
  }

  .unfriend-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .action-block {
    margin-top: 12px;
  }

  .primary-btn {
    font-size: 13px;
    padding: 6px 12px;
    border-radius: 4px;
    border: 1px solid var(--accent, #5865f2);
    background: var(--accent, #5865f2);
    color: #fff;
    cursor: pointer;
  }

  .primary-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .secondary-btn {
    font-size: 13px;
    padding: 6px 12px;
    border-radius: 4px;
    border: 1px solid var(--border, #555);
    background: transparent;
    color: var(--text-primary);
    cursor: pointer;
  }

  .generated-row,
  .add-row {
    display: flex;
    gap: 6px;
    margin-top: 8px;
  }

  .url-input {
    flex: 1;
    min-width: 0;
    font-size: 12px;
    padding: 6px 8px;
    border-radius: 4px;
    border: 1px solid var(--border, #555);
    background: var(--bg-tertiary, #1e1e1e);
    color: var(--text-primary);
    font-family: var(--font-mono, monospace);
  }

  .add-label {
    display: block;
    font-size: 12px;
    color: var(--text-secondary);
    margin-bottom: 2px;
  }

  .muted {
    font-size: 12px;
    color: var(--text-secondary);
    margin: 4px 0;
  }

  .error-text {
    font-size: 12px;
    color: #d83c3e;
    margin: 4px 0 8px;
  }
</style>
