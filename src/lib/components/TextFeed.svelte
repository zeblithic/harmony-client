<script lang="ts">
  import type { Message, MessagePriority } from '../types';
  import { groupMessages } from '../feed-utils';
  import TextMessage from './TextMessage.svelte';
  import QuietMessageGroup from './QuietMessageGroup.svelte';
  import ComposeBar from './ComposeBar.svelte';

  let { messages, collapsed = false, onMediaClick, onSend }: {
    messages: Message[];
    collapsed?: boolean;
    onMediaClick?: (mediaId: string) => void;
    onSend?: (text: string, priority: MessagePriority) => void;
  } = $props();

  let feedItems = $derived(groupMessages(messages));
</script>

<div class="text-feed">
  <div class="messages-scroll">
    {#each feedItems as item, i (item.kind === 'message' ? item.message.id : `quiet-${i}`)}
      {#if item.kind === 'message'}
        <TextMessage message={item.message} {collapsed} {onMediaClick} />
      {:else}
        <QuietMessageGroup messages={item.messages} />
      {/if}
    {/each}
  </div>
  <ComposeBar {onSend} />
</div>

<style>
  .text-feed {
    display: flex;
    flex-direction: column;
    height: 100%;
  }

  .messages-scroll {
    flex: 1;
    overflow-y: auto;
    padding: 8px 0;
  }
</style>
