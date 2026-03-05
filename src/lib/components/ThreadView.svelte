<script lang="ts">
  import type { Message, MessagePriority } from '../types';
  import type { TrustService } from '../trust-service';
  import TextMessage from './TextMessage.svelte';
  import ComposeBar from './ComposeBar.svelte';

  let { rootMessage, replies, onClose, onSend, onAvatarClick, trustService, trustVersion = 0, onScrollToMessage }: {
    rootMessage: Message;
    replies: Message[];
    onClose?: () => void;
    onSend?: (text: string, priority: MessagePriority) => void;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    trustService?: TrustService;
    trustVersion?: number;
    onScrollToMessage?: (messageId: string) => void;
  } = $props();

  let allThreadMessages = $derived([rootMessage, ...replies]);

  let rootPreview = $derived(
    rootMessage.text.slice(0, 40) + (rootMessage.text.length > 40 ? '...' : '')
  );
</script>

<div
  class="thread-view"
  role="complementary"
  aria-label="Thread: {rootPreview}"
>
  <div class="thread-header">
    <span class="thread-title">Thread</span>
    <button class="thread-close" onclick={() => onClose?.()} aria-label="Close thread">×</button>
  </div>

  <div class="thread-messages">
    <div class="thread-root">
      <TextMessage message={rootMessage} {onAvatarClick} {trustService} {trustVersion} />
    </div>

    {#each replies as reply (reply.id)}
      <TextMessage message={reply} {onAvatarClick} {trustService} {trustVersion} allMessages={allThreadMessages} {onScrollToMessage} />
    {/each}
  </div>

  <ComposeBar {onSend} channelName="Reply in thread" />
</div>

<style>
  .thread-view {
    display: flex;
    flex-direction: column;
    height: 100%;
    border-top: 1px solid var(--border);
  }

  .thread-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 8px 16px;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }

  .thread-title {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
  }

  .thread-close {
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 18px;
    cursor: pointer;
    padding: 2px 6px;
    border-radius: 4px;
  }

  .thread-close:hover {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }

  .thread-messages {
    flex: 1;
    overflow-y: auto;
    padding: 8px 0;
  }

  .thread-root {
    border-bottom: 1px solid var(--border);
    padding-bottom: 8px;
    margin-bottom: 4px;
    background: rgba(88, 101, 242, 0.04);
  }
</style>
