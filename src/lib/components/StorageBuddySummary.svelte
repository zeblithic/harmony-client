<script lang="ts">
  import type { StorageBuddy } from '../types';

  let {
    buddies = [],
    onManageBuddies,
  }: {
    buddies?: StorageBuddy[];
    onManageBuddies?: () => void;
  } = $props();

  let buddyCount = $derived(buddies.length);
  let onlineCount = $derived(buddies.filter((b) => b.online).length);
  let buddyLabel = $derived(buddyCount === 1 ? 'buddy' : 'buddies');
</script>

<section class="storage-buddy-summary" aria-label="Storage buddies">
  <div class="buddy-info">
    <span class="buddy-count">{buddyCount} {buddyLabel}</span>
    <span class="buddy-separator">&middot;</span>
    <span class="buddy-online">
      <span class="online-dot" class:has-online={onlineCount > 0}></span>
      {onlineCount} online
    </span>
  </div>
  <button
    type="button"
    class="manage-btn"
    onclick={() => onManageBuddies?.()}
  >
    Manage
  </button>
</section>

<style>
  .storage-buddy-summary {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 8px 12px;
    border-top: 1px solid var(--border, #3f4147);
  }

  .buddy-info {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 0.8rem;
    color: var(--text-secondary, #b5bac1);
  }

  .buddy-count {
    font-weight: 600;
    color: var(--text-primary, #f2f3f5);
  }

  .buddy-separator {
    color: var(--text-muted, #949ba4);
  }

  .buddy-online {
    display: flex;
    align-items: center;
    gap: 4px;
  }

  .online-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--text-muted, #949ba4);
  }

  .online-dot.has-online {
    background: #23a55a;
  }

  .manage-btn {
    padding: 3px 10px;
    border: 1px solid var(--border, #3f4147);
    border-radius: 4px;
    background: none;
    color: var(--text-secondary, #b5bac1);
    font-size: 0.75rem;
    cursor: pointer;
  }

  .manage-btn:hover {
    background: var(--bg-tertiary, #313338);
    color: var(--text-primary, #f2f3f5);
  }
</style>
