<script lang="ts">
  import type { InboxEntry } from '../types';

  let {
    entries = [],
    selectedCid = null,
    onSelect,
  }: {
    entries: InboxEntry[];
    selectedCid: string | null;
    onSelect: (cid: string) => void;
  } = $props();

  function formatTime(timestamp: number): string {
    const date = new Date(timestamp * 1000);
    const now = new Date();
    const diffMs = now.getTime() - date.getTime();
    const diffDays = Math.floor(diffMs / (1000 * 60 * 60 * 24));

    if (diffDays === 0) {
      return date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    } else if (diffDays === 1) {
      return 'Yesterday';
    } else if (diffDays < 7) {
      return date.toLocaleDateString([], { weekday: 'short' });
    } else {
      return date.toLocaleDateString([], { month: 'short', day: 'numeric' });
    }
  }

  function truncateAddress(addr: string): string {
    if (addr.length <= 12) return addr;
    return addr.slice(0, 6) + '...' + addr.slice(-4);
  }
</script>

<div class="inbox-list">
  {#if entries.length === 0}
    <div class="empty-state">
      <p>No messages yet</p>
    </div>
  {:else}
    {#each entries as entry (entry.messageCid)}
      <button
        class="inbox-entry"
        class:selected={selectedCid === entry.messageCid}
        class:unread={!entry.read}
        onclick={() => onSelect(entry.messageCid)}
      >
        <div class="entry-left">
          {#if !entry.read}
            <span class="unread-dot"></span>
          {:else}
            <span class="read-spacer"></span>
          {/if}
        </div>
        <div class="entry-content">
          <div class="entry-header">
            <span class="sender">{truncateAddress(entry.senderAddress)}</span>
            <span class="timestamp">{formatTime(entry.timestamp)}</span>
          </div>
          <div class="subject">{entry.subjectSnippet || '(no subject)'}</div>
        </div>
      </button>
    {/each}
  {/if}
</div>

<style>
  .inbox-list {
    display: flex;
    flex-direction: column;
    overflow-y: auto;
    height: 100%;
  }

  .empty-state {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--text-muted, #949ba4);
  }

  .inbox-entry {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    padding: 10px 12px;
    border: none;
    border-bottom: 1px solid var(--border, #2a2d31);
    background: transparent;
    cursor: pointer;
    text-align: left;
    width: 100%;
    color: var(--text-primary, #dbdee1);
    transition: background 0.15s ease;
  }

  .inbox-entry:hover {
    background: var(--bg-hover, rgba(255, 255, 255, 0.04));
  }

  .inbox-entry.selected {
    background: var(--bg-active, rgba(88, 101, 242, 0.1));
    border-left: 3px solid var(--accent, #5865f2);
    padding-left: 9px;
  }

  .inbox-entry.unread .sender {
    font-weight: 600;
  }

  .entry-left {
    flex-shrink: 0;
    width: 10px;
    padding-top: 4px;
  }

  .unread-dot {
    display: block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--accent, #5865f2);
  }

  .read-spacer {
    display: block;
    width: 8px;
    height: 8px;
  }

  .entry-content {
    flex: 1;
    min-width: 0;
  }

  .entry-header {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 8px;
    margin-bottom: 2px;
  }

  .sender {
    font-size: 13px;
    color: var(--text-primary, #dbdee1);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .timestamp {
    font-size: 11px;
    color: var(--text-muted, #949ba4);
    flex-shrink: 0;
  }

  .subject {
    font-size: 12px;
    color: var(--text-secondary, #b5bac1);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
</style>
