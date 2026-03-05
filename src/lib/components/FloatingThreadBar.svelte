<script lang="ts">
  import type { ThreadMetaEntry } from '../feed-utils';

  let { threadMeta, pinnedThreadIds, visibleThreadIds, rootMessages, openThreadId = null, onThreadOpen }: {
    threadMeta: Map<string, ThreadMetaEntry>;
    pinnedThreadIds: Set<string>;
    visibleThreadIds: Set<string>;
    rootMessages: Map<string, { sender: string; text: string }>;
    openThreadId?: string | null;
    onThreadOpen?: (rootId: string) => void;
  } = $props();

  let entries = $derived.by(() => {
    const result: { id: string; sender: string; text: string; count: number; pinned: boolean }[] = [];
    const seen = new Set<string>();

    // Pinned threads always show
    for (const id of pinnedThreadIds) {
      const meta = threadMeta.get(id);
      const root = rootMessages.get(id);
      if (meta && root) {
        result.push({ id, sender: root.sender, text: root.text, count: meta.count, pinned: true });
        seen.add(id);
      }
    }

    // Auto-float: threads scrolled out of view, max 3
    let autoCount = 0;
    for (const [id, meta] of threadMeta) {
      if (autoCount >= 3) break;
      if (seen.has(id)) continue;
      if (visibleThreadIds.has(id)) continue;
      const root = rootMessages.get(id);
      if (!root) continue;
      result.push({ id, sender: root.sender, text: root.text, count: meta.count, pinned: false });
      autoCount++;
    }

    return result;
  });

  let hasEntries = $derived(entries.length > 0);
</script>

{#if hasEntries}
  <div class="floating-thread-bar">
    {#each entries as entry (entry.id)}
      <button
        class="thread-entry"
        class:active={openThreadId === entry.id}
        onclick={() => onThreadOpen?.(entry.id)}
        aria-label="{entry.pinned ? 'Pinned thread' : 'Thread'} by {entry.sender}: {entry.text.slice(0, 30)}, {entry.count} {entry.count === 1 ? 'reply' : 'replies'}"
      >
        {#if entry.pinned}<span class="pin-icon">📌</span>{/if}
        <span class="entry-sender">{entry.sender}</span>
        <span class="entry-text">{entry.text.slice(0, 30)}{entry.text.length > 30 ? '...' : ''}</span>
        <span class="entry-count">({entry.count})</span>
      </button>
    {/each}
  </div>
{/if}

<style>
  .floating-thread-bar {
    display: flex;
    gap: 4px;
    padding: 6px 12px;
    border-bottom: 1px solid var(--border);
    background: var(--bg-secondary);
    flex-shrink: 0;
    overflow-x: auto;
  }

  .thread-entry {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 4px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    font-size: 11px;
    cursor: pointer;
    white-space: nowrap;
    flex-shrink: 0;
  }

  .thread-entry:hover {
    border-color: var(--accent);
    color: var(--accent);
  }

  .thread-entry.active {
    border-color: var(--accent);
    background: rgba(88, 101, 242, 0.1);
    color: var(--accent);
  }

  .entry-sender {
    font-weight: 600;
  }

  .entry-text {
    color: var(--text-muted);
    max-width: 120px;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .entry-count {
    color: var(--text-muted);
    font-size: 10px;
  }

  .pin-icon {
    font-size: 10px;
  }
</style>
