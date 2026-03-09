<script lang="ts">
  import type { StorageBuddy, PeerRef } from '../types';
  import { formatBytes } from '../file-utils';
  import Avatar from './Avatar.svelte';

  let {
    buddies,
    availablePeers,
    onAdd,
    onRemove,
  }: {
    buddies: StorageBuddy[];
    availablePeers: PeerRef[];
    onAdd: (peer: PeerRef) => void;
    onRemove: (buddy: StorageBuddy) => void;
  } = $props();

  let buddyAddresses = $derived(new Set(buddies.map((b) => b.address)));
  let pickablePeers = $derived(
    availablePeers.filter((p) => !buddyAddresses.has(p.address)),
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

<section class="buddy-list" aria-label="Stored by (encrypted)">
  <div class="buddy-list-header">
    <span class="header-icon" aria-hidden="true">&#x2709;&#xFE0F;</span>
    <h3>Stored by <span class="header-detail">(encrypted)</span></h3>
  </div>

  {#if buddies.length === 0}
    <p class="empty-state">No storage buddies</p>
  {:else}
    <ul class="buddy-peer-list">
      {#each buddies as buddy (buddy.address)}
        <li class="buddy-row">
          <Avatar address={buddy.address} displayName={buddy.displayName} size={28} />
          <span class="buddy-name">{buddy.displayName}</span>
          <span class="buddy-storage">storing {formatBytes(buddy.storageUsedBytes)}</span>
          <span class="buddy-icon" aria-hidden="true">&#x2709;&#xFE0F;</span>
          <span
            class="online-indicator"
            class:online={buddy.online}
            aria-label="{buddy.displayName} is {buddy.online ? 'online' : 'offline'}"
          ></span>
          <button
            type="button"
            class="remove-btn"
            aria-label="Remove {buddy.displayName}"
            title="Replicas will need reassignment after removal"
            onclick={() => onRemove(buddy)}
          >
            Remove
          </button>
        </li>
      {/each}
    </ul>
  {/if}

  {#if pickablePeers.length > 0}
    <div class="picker-row">
      <label for="buddy-picker" class="visually-hidden">Add buddy</label>
      <select
        id="buddy-picker"
        aria-label="Add buddy"
        class="peer-picker"
        onchange={handlePickerChange}
      >
        <option value="" disabled selected>Add buddy...</option>
        {#each pickablePeers as peer (peer.address)}
          <option value={peer.address}>{peer.displayName}</option>
        {/each}
      </select>
    </div>
  {/if}
</section>

<style>
  .buddy-list {
    padding: 4px 0;
  }

  .buddy-list-header {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-bottom: 8px;
  }

  .buddy-list-header h3 {
    margin: 0;
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--text-primary, #f2f3f5);
  }

  .header-icon {
    font-size: 0.9rem;
  }

  .header-detail {
    color: var(--text-secondary, #b5bac1);
    font-weight: 400;
  }

  .empty-state {
    font-size: 0.8rem;
    color: var(--text-muted, #949ba4);
    margin: 4px 0;
    font-style: italic;
  }

  .buddy-peer-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .buddy-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 8px;
    border-radius: 4px;
    background: var(--buddy-bg, rgba(35, 165, 90, 0.08));
  }

  .buddy-row:hover {
    background: var(--buddy-bg-hover, rgba(35, 165, 90, 0.14));
  }

  .buddy-name {
    font-size: 0.85rem;
    color: var(--text-primary, #f2f3f5);
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .buddy-storage {
    font-size: 0.75rem;
    color: var(--text-secondary, #b5bac1);
    flex-shrink: 0;
  }

  .buddy-icon {
    font-size: 0.75rem;
    flex-shrink: 0;
  }

  .online-indicator {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--text-muted, #949ba4);
    flex-shrink: 0;
  }

  .online-indicator.online {
    background: #23a55a;
  }

  .remove-btn {
    padding: 2px 8px;
    border: 1px solid var(--border, #3f4147);
    border-radius: 4px;
    background: none;
    color: var(--text-secondary, #b5bac1);
    font-size: 0.7rem;
    cursor: pointer;
    flex-shrink: 0;
  }

  .remove-btn:hover {
    background: var(--bg-tertiary, #313338);
    color: var(--text-primary, #f2f3f5);
  }

  .picker-row {
    margin-top: 8px;
  }

  .peer-picker {
    width: 100%;
    padding: 4px 8px;
    border: 1px solid var(--border, #3f4147);
    border-radius: 4px;
    background: var(--bg-tertiary, #313338);
    color: var(--text-primary, #f2f3f5);
    font-size: 0.8rem;
    cursor: pointer;
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
