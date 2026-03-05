<script lang="ts">
  import type { Peer } from '../types';

  let { count, participants, rootId, isOpen = false, onOpen }: {
    count: number;
    participants: Peer[];
    rootId: string;
    isOpen?: boolean;
    onOpen?: (rootId: string) => void;
  } = $props();

  let nameList = $derived.by(() => {
    const names = participants.slice(0, 3).map(p => p.displayName);
    const extra = participants.length - 3;
    if (extra > 0) names.push(`+${extra} more`);
    return names.join(', ');
  });

  let replyWord = $derived(count === 1 ? 'reply' : 'replies');

  let ariaLabel = $derived(
    `${count} ${replyWord} from ${participants.map(p => p.displayName).join(', ')}. Open thread.`
  );
</script>

<button
  class="thread-indicator"
  class:open={isOpen}
  onclick={() => onOpen?.(rootId)}
  aria-label={ariaLabel}
  aria-expanded={isOpen}
>
  <span class="thread-info">
    💬 {count} {replyWord} · {nameList}
  </span>
</button>

<style>
  .thread-indicator {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 4px 8px;
    margin-top: 4px;
    border: none;
    border-radius: 4px;
    background: none;
    color: var(--text-muted);
    font-size: 12px;
    cursor: pointer;
  }

  .thread-indicator:hover {
    background: var(--bg-tertiary);
    color: var(--accent);
  }

  .thread-indicator.open {
    color: var(--accent);
  }

  .thread-info {
    white-space: nowrap;
  }
</style>
