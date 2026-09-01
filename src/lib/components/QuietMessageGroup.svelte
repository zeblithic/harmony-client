<script lang="ts">
  import type { Message } from '../types';
  import TextMessage from './TextMessage.svelte';
  import { resolveAuthorLabel } from '../mention-render';
  import type { ResolvedCard } from '../member-card-service';

  let {
    messages,
    onAvatarClick,
    resolveNickname,
    resolveCard,
  }: {
    messages: Message[];
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    // ZEB-962: resolve author names for the summary AND forward to child
    // TextMessages, so a blank DM sender name never renders raw.
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
  } = $props();

  let expanded = $state(false);

  let messageIds = $derived(new Set(messages.map((m) => m.id)));

  $effect(() => {
    function onReveal(e: Event) {
      const id = (e as CustomEvent<string>).detail;
      if (messageIds.has(id)) expanded = true;
    }
    document.addEventListener('reveal-message', onReveal);
    return () => document.removeEventListener('reveal-message', onReveal);
  });

  let senderNames = $derived(
    [
      ...new Set(messages.map((m) => resolveAuthorLabel(m.sender, resolveNickname, resolveCard).label)),
    ].join(', ')
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
        <TextMessage {message} {onAvatarClick} {resolveNickname} {resolveCard} />
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
