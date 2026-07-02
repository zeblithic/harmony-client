<script lang="ts">
  import type { PeerRef } from '../types';
  import Avatar from './Avatar.svelte';

  let {
    sharedWith,
    availablePeers,
    onAdd,
    onRemove,
  }: {
    sharedWith: PeerRef[];
    availablePeers: PeerRef[];
    onAdd: (peer: PeerRef) => void;
    onRemove: (peer: PeerRef) => void;
  } = $props();

  let sharedAddresses = $derived(new Set(sharedWith.map((p) => p.address)));
  let pickablePeers = $derived(
    availablePeers.filter((p) => !sharedAddresses.has(p.address)),
  );

  function handlePickerChange(e: Event) {
    const select = e.target as HTMLSelectElement;
    const address = select.value;
    if (!address) return;
    const peer = availablePeers.find((p) => p.address === address);
    if (peer) {
      onAdd(peer);
    }
    select.value = '';
  }
</script>

<section class="share-list" aria-label="Shared with (can view)">
  <div class="share-list-header">
    <span class="header-icon" aria-hidden="true">&#x1F513;</span>
    <h3>Shared with <span class="header-detail">(can view)</span></h3>
  </div>

  {#if sharedWith.length === 0}
    <p class="empty-state">Not shared with anyone</p>
  {:else}
    <ul class="share-peer-list">
      {#each sharedWith as peer (peer.address)}
        <li class="peer-row">
          <Avatar address={peer.address} displayName={peer.displayName} size={28} />
          <span class="peer-name">{peer.displayName}</span>
          <span class="peer-role">can view</span>
          <span class="peer-icon" aria-hidden="true">&#x1F513;</span>
          <button
            type="button"
            class="remove-btn"
            aria-label="Remove {peer.displayName}"
            onclick={() => onRemove(peer)}
          >
            Remove
          </button>
        </li>
      {/each}
    </ul>
  {/if}

  {#if pickablePeers.length > 0}
    <div class="picker-row">
      <label for="share-picker" class="visually-hidden">Share with...</label>
      <select
        id="share-picker"
        aria-label="Share with..."
        class="peer-picker"
        onchange={handlePickerChange}
      >
        <option value="" disabled selected>Share with...</option>
        {#each pickablePeers as peer (peer.address)}
          <option value={peer.address}>{peer.displayName}</option>
        {/each}
      </select>
    </div>
  {/if}
</section>

<style>
  .share-list {
    padding: 4px 0;
  }

  .share-list-header {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-bottom: 8px;
  }

  .share-list-header h3 {
    margin: 0;
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--text-primary);
  }

  .header-icon {
    font-size: 0.9rem;
  }

  .header-detail {
    color: var(--text-secondary);
    font-weight: 400;
  }

  .empty-state {
    font-size: 0.8rem;
    color: var(--text-muted);
    margin: 4px 0;
    font-style: italic;
  }

  .share-peer-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .peer-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 8px;
    border-radius: 4px;
    background: var(--share-bg, rgba(88, 101, 242, 0.08));
  }

  .peer-row:hover {
    background: var(--share-bg-hover, rgba(88, 101, 242, 0.14));
  }

  .peer-name {
    font-size: 0.85rem;
    color: var(--text-primary);
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .peer-role {
    font-size: 0.75rem;
    color: var(--text-secondary);
    flex-shrink: 0;
  }

  .peer-icon {
    font-size: 0.75rem;
    flex-shrink: 0;
  }

  .remove-btn {
    padding: 2px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: none;
    color: var(--text-secondary);
    font-size: 0.7rem;
    cursor: pointer;
    flex-shrink: 0;
  }

  .remove-btn:hover {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }

  .remove-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .picker-row {
    margin-top: 8px;
  }

  .peer-picker {
    width: 100%;
    padding: 4px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-size: 0.8rem;
    cursor: pointer;
  }

  .peer-picker:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    margin: -1px;
    padding: 0;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }
</style>
