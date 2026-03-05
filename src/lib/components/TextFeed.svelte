<script lang="ts">
  import type { Message, MessagePriority } from '../types';
  import type { TrustService } from '../trust-service';
  import type { ThreadMetaEntry } from '../feed-utils';
  import { groupMessages } from '../feed-utils';
  import TextMessage from './TextMessage.svelte';
  import QuietMessageGroup from './QuietMessageGroup.svelte';
  import ComposeBar from './ComposeBar.svelte';
  import ThreadView from './ThreadView.svelte';
  import ThreadIndicator from './ThreadIndicator.svelte';
  import FloatingThreadBar from './FloatingThreadBar.svelte';

  let {
    messages,
    collapsed = false,
    onMediaClick,
    onSend,
    onAvatarClick,
    trustService,
    trustVersion = 0,
    threadRoot = null,
    threadReplies = [],
    threadMeta = new Map(),
    openThreadId = null,
    onThreadOpen,
    onThreadClose,
    onThreadSend,
    onScrollToMessage,
    pinnedThreadIds = new Set(),
  }: {
    messages: Message[];
    collapsed?: boolean;
    onMediaClick?: (mediaId: string) => void;
    onSend?: (text: string, priority: MessagePriority) => void;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    trustService?: TrustService;
    trustVersion?: number;
    threadRoot?: Message | null;
    threadReplies?: Message[];
    threadMeta?: Map<string, ThreadMetaEntry>;
    openThreadId?: string | null;
    onThreadOpen?: (rootId: string) => void;
    onThreadClose?: () => void;
    onThreadSend?: (text: string, priority: MessagePriority) => void;
    onScrollToMessage?: (messageId: string) => void;
    pinnedThreadIds?: Set<string>;
  } = $props();

  let visibleThreadIds = $state(new Set<string>());

  function handleThreadVisibility(rootId: string, visible: boolean) {
    const next = new Set(visibleThreadIds);
    if (visible) next.add(rootId);
    else next.delete(rootId);
    visibleThreadIds = next;
  }

  let feedItems = $derived(groupMessages(messages));

  // Drag handle state
  let splitPercent = $state(60);
  let isDragging = $state(false);
  let containerEl: HTMLDivElement | undefined = $state();

  function handleDragStart(e: MouseEvent) {
    e.preventDefault();
    isDragging = true;
  }

  function handleDragMove(e: MouseEvent) {
    if (!isDragging || !containerEl) return;
    const rect = containerEl.getBoundingClientRect();
    const pct = ((e.clientY - rect.top) / rect.height) * 100;
    splitPercent = Math.max(20, Math.min(80, pct));
  }

  function handleDragEnd() {
    isDragging = false;
  }

  function handleDragKeyDown(e: KeyboardEvent) {
    if (e.key === 'ArrowUp') {
      e.preventDefault();
      splitPercent = Math.max(20, splitPercent - 5);
    } else if (e.key === 'ArrowDown') {
      e.preventDefault();
      splitPercent = Math.min(80, splitPercent + 5);
    }
  }

  let rootMessages = $derived.by(() => {
    const map = new Map<string, { sender: string; text: string }>();
    for (const [id, _meta] of threadMeta) {
      const rootMsg = messages.find(m => m.id === id);
      if (rootMsg) {
        map.set(id, { sender: rootMsg.sender.displayName, text: rootMsg.text });
      }
    }
    return map;
  });

  let showThreadPanel = $derived(threadRoot !== null && openThreadId !== null);

  function handleGlobalKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape' && showThreadPanel) {
      e.preventDefault();
      onThreadClose?.();
    }
  }
</script>

<svelte:window onkeydown={handleGlobalKeyDown} />

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
  class="text-feed"
  class:dragging={isDragging}
  bind:this={containerEl}
  onmousemove={handleDragMove}
  onmouseup={handleDragEnd}
  onmouseleave={handleDragEnd}
>
  <div class="main-section" style={showThreadPanel ? `flex: 0 0 ${splitPercent}%` : ''}>
    <FloatingThreadBar
      {threadMeta}
      {pinnedThreadIds}
      {visibleThreadIds}
      {rootMessages}
      {openThreadId}
      onThreadOpen={onThreadOpen}
    />
    <div class="messages-scroll">
      {#each feedItems as item (item.kind === 'message' ? item.message.id : `quiet-${item.messages[0].id}`)}
        {#if item.kind === 'message'}
          <TextMessage
            message={item.message}
            {collapsed}
            {onMediaClick}
            {onAvatarClick}
            {trustService}
            {trustVersion}
            allMessages={messages}
            {onScrollToMessage}
          />
          {#if threadMeta.has(item.message.id)}
            {@const meta = threadMeta.get(item.message.id)!}
            <ThreadIndicator
              count={meta.count}
              participants={meta.participants}
              rootId={item.message.id}
              isOpen={openThreadId === item.message.id}
              onOpen={onThreadOpen}
              onVisibilityChange={handleThreadVisibility}
            />
          {/if}
        {:else}
          <QuietMessageGroup messages={item.messages} {collapsed} {onMediaClick} {onAvatarClick} {trustService} {trustVersion} />
        {/if}
      {/each}
    </div>
    <ComposeBar {onSend} />
  </div>

  {#if showThreadPanel && threadRoot}
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div
      class="drag-handle"
      role="separator"
      aria-orientation="horizontal"
      aria-valuemin={20}
      aria-valuemax={80}
      aria-valuenow={Math.round(splitPercent)}
      aria-label="Resize thread panel"
      tabindex="0"
      onmousedown={handleDragStart}
      onkeydown={handleDragKeyDown}
    ></div>
    <div class="thread-section">
      <ThreadView
        rootMessage={threadRoot}
        replies={threadReplies}
        onClose={onThreadClose}
        onSend={onThreadSend}
        {onAvatarClick}
        {trustService}
        {trustVersion}
        {onScrollToMessage}
      />
    </div>
  {/if}
</div>

<style>
  .text-feed {
    display: flex;
    flex-direction: column;
    height: 100%;
  }

  .main-section {
    display: flex;
    flex-direction: column;
    min-height: 0;
    flex: 1;
  }

  .messages-scroll {
    flex: 1;
    overflow-y: auto;
    padding: 8px 0;
  }

  .drag-handle {
    height: 4px;
    background: var(--border);
    cursor: row-resize;
    flex-shrink: 0;
    transition: background 0.15s;
  }

  .drag-handle:hover,
  .dragging .drag-handle {
    background: var(--accent);
  }

  .thread-section {
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }

  .text-feed.dragging {
    user-select: none;
  }
</style>
