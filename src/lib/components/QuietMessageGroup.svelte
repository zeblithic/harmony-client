<script lang="ts">
  import type { Message } from '../types';
  import TextMessage from './TextMessage.svelte';

  let { messages, collapsed = false, onMediaClick }: {
    messages: Message[];
    collapsed?: boolean;
    onMediaClick?: (mediaId: string) => void;
  } = $props();

  let expanded = $state(false);

  let senderNames = $derived(
    [...new Set(messages.map((m) => m.sender.displayName))].join(', ')
  );

  let summary = $derived(
    `🔇 ${messages.length} quiet message${messages.length === 1 ? '' : 's'} from ${senderNames}`
  );
</script>

<div class="quiet-group">
  <button class="quiet-toggle" onclick={() => { expanded = !expanded; }}>
    <span class="quiet-summary">{summary}</span>
    <span class="quiet-chevron">{expanded ? '▾' : '▸'}</span>
  </button>
  {#if expanded}
    <div class="quiet-expanded">
      {#each messages as message (message.id)}
        <TextMessage {message} {collapsed} {onMediaClick} />
      {/each}
    </div>
  {/if}
</div>

<style>
  .quiet-group {
    border-left: 2px solid var(--border);
    margin: 2px 0;
  }

  .quiet-toggle {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 4px 16px;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 12px;
    cursor: pointer;
    text-align: left;
  }

  .quiet-toggle:hover {
    background: var(--bg-secondary);
    color: var(--text-secondary);
  }

  .quiet-chevron {
    font-size: 10px;
  }

  .quiet-expanded {
    opacity: 0.6;
  }
</style>
