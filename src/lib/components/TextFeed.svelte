<script lang="ts">
  import type { Message } from '../types';
  import TextMessage from './TextMessage.svelte';
  import ComposeBar from './ComposeBar.svelte';

  let { messages, collapsed = false, onMediaClick }: {
    messages: Message[];
    collapsed?: boolean;
    onMediaClick?: (mediaId: string) => void;
  } = $props();
</script>

<div class="text-feed">
  <div class="messages-scroll">
    {#each messages as message (message.id)}
      <TextMessage {message} {collapsed} {onMediaClick} />
    {/each}
  </div>
  <ComposeBar />
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
