<script lang="ts">
  import type { Message, MessagePriority } from '../types';
  import type { TrustService } from '../trust-service';
  import type { ThreadMetaEntry } from '../feed-utils';
  import type { ResolvedCard } from '../member-card-service';
  import { groupMessages } from '../feed-utils';
  import { resolveAuthorLabel } from '../mention-render';
  import TextMessage from './TextMessage.svelte';
  import CallEventLine from './CallEventLine.svelte';
  import QuietMessageGroup from './QuietMessageGroup.svelte';
  import ComposeBar from './ComposeBar.svelte';
  import ThreadView from './ThreadView.svelte';
  import ThreadIndicator from './ThreadIndicator.svelte';
  import FloatingThreadBar from './FloatingThreadBar.svelte';
  import GroupCallBanner from './GroupCallBanner.svelte';
  import type { GroupCallSession } from '../group-call-session';

  let {
    messages,
    collapsed = false,
    channelName = 'general',
    channelType = 'channel',
    channelId = '',
    onMediaClick,
    onSend,
    onAvatarClick,
    onStartCall,
    onStartGroupCall,
    onJoinGroupCall,
    groupCallActive = false,
    groupCallSelf = false,
    groupCallBusy = false,
    groupCall = null,
    groupCallInvoke,
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
    onMessageDelete,
    ownAddress = '',
    resolveNickname,
    resolveCard,
    readReceiptOn = false,
    onToggleReadReceipt,
    peerReadUpToMs,
    peerSeenAtMs,
  }: {
    messages: Message[];
    collapsed?: boolean;
    channelName?: string;
    channelType?: 'channel' | 'dm' | 'group-chat';
    /** The space/channel id for this feed. Used to initiate a DM call. */
    channelId?: string;
    onMediaClick?: (mediaId: string) => void;
    onSend?: (text: string, priority: MessagePriority) => void;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    /** ZEB-352: invoked when the user clicks "Call" in the DM header. Only
     *  rendered when channelType === 'dm'. */
    onStartCall?: (spaceId: string) => void;
    /** ZEB-360 T13: place a new group-DM call. Rendered in the group-chat header
     *  when no call is active for this space. */
    onStartGroupCall?: (spaceId: string) => void;
    /** ZEB-360 T13: join the active group-DM call for this space. Rendered when a
     *  call exists for this space and the local user isn't in it. */
    onJoinGroupCall?: (spaceId: string) => void;
    /** ZEB-360 T13: true when a group call is in progress in THIS space (drives
     *  Call vs Join in the header). */
    groupCallActive?: boolean;
    /** ZEB-360 T13: true when the local user is in THIS space's group call (hides
     *  the header button — the in-call bar takes over). */
    groupCallSelf?: boolean;
    /** ZEB-360 T13: true when any voice session is active (disables the header
     *  Call/Join button — one media engine at a time). */
    groupCallBusy?: boolean;
    /** ZEB-360 T13: the group-call session singleton (threaded to the banner). */
    groupCall?: GroupCallSession | null;
    /** ZEB-360 T13: Tauri invoke, threaded to the banner's Join path. */
    groupCallInvoke?: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
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
    /** ZEB-228 Phase 4 Task 14: invoked when the user clicks the
     *  inline ⓧ on a stuck/expired self-Message. Parent shows the
     *  ConfirmDialog and dispatches `delete_outbox_entry`. */
    onMessageDelete?: (messageId: string) => void;
    /** ZEB-228 Phase 4 Task 14: local user's address; used to compute
     *  `isSelf` for each rendered TextMessage so the delete button is
     *  only shown on outgoing messages. */
    ownAddress?: string;
    /** ZEB-839: local friend nickname for an owner_id — top rung of the author
     *  ladder, threaded down to every TextMessage. DM senders carry no baked
     *  name, so without these the feed can only render a truncated hex. */
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    /** ZEB-839: broadcast profile card for an owner_id — second rung of the
     *  author ladder. Same pure-consumer contract as `resolveNickname`. */
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    /** ZEB-214: whether this 1:1 DM has read-receipt broadcast enabled (drives
     *  the header toggle state). */
    readReceiptOn?: boolean;
    /** ZEB-214: toggle read-receipt broadcast for this DM. */
    onToggleReadReceipt?: (on: boolean) => void;
    /** ZEB-214: the peer's read-watermark (ms) — own messages at/below it show "Seen". */
    peerReadUpToMs?: number;
    /** ZEB-214: the peer's read time (ms) — the "Seen HH:MM" clock. */
    peerSeenAtMs?: number;
  } = $props();

  // Fallback invoke for the group-call banner when the parent doesn't thread one
  // (e.g. unit tests rendering TextFeed in isolation). Rejects so the banner's
  // Join path no-ops gracefully rather than calling `undefined(...)`.
  const noopInvoke = (_cmd: string, _args?: Record<string, unknown>) =>
    Promise.reject(new Error('invoke not wired'));

  let visibleThreadIds = $state(new Set<string>());

  function handleThreadVisibility(rootId: string, visible: boolean) {
    if (visible === visibleThreadIds.has(rootId)) return;
    const next = new Set(visibleThreadIds);
    if (visible) next.add(rootId);
    else next.delete(rootId);
    visibleThreadIds = next;
  }

  let feedItems = $derived(groupMessages(messages));

  // ZEB-214: the newest own-sent message at or below the peer's read watermark
  // (1:1 DM only) — the single message that shows a "Seen HH:MM" line.
  let seenMessageId = $derived.by(() => {
    if (channelType !== 'dm' || peerReadUpToMs === undefined) return null;
    const isOwn = (m: Message) =>
      m.sender.address === 'self' || (ownAddress !== '' && m.sender.address === ownAddress);
    let best: Message | null = null;
    for (const m of messages) {
      if (isOwn(m) && m.timestamp <= peerReadUpToMs && (!best || m.timestamp > best.timestamp)) {
        best = m;
      }
    }
    return best?.id ?? null;
  });

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
        // ZEB-839: same render-time ladder as the bubbles — a baked
        // `sender.displayName` is absent on DM authors.
        map.set(id, {
          sender: resolveAuthorLabel(rootMsg.sender, resolveNickname, resolveCard),
          text: rootMsg.text,
        });
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
    {#if channelType === 'dm'}
      <!-- ZEB-360 D6: the 1:1 Call button is disabled while ANY voice session is
           active (community voice, another DM call, or a group call) — one media
           engine at a time, symmetric with the group Call/Join buttons below. -->
      <div class="dm-header">
        <span class="dm-name">{channelName}</span>
        <!-- ZEB-214: opt-in read receipts for this DM (off by default). -->
        <button
          type="button"
          class="btn-receipt"
          data-testid="read-receipt-toggle"
          aria-pressed={readReceiptOn}
          title={readReceiptOn
            ? 'Read receipts on — click to stop sending'
            : 'Read receipts off — click to send'}
          onclick={() => onToggleReadReceipt?.(!readReceiptOn)}
        >
          {readReceiptOn ? '👁 Receipts on' : '👁 Receipts off'}
        </button>
        <button
          type="button"
          class="btn-call"
          aria-label="Start call"
          disabled={groupCallBusy}
          onclick={() => onStartCall?.(channelId)}
        >
          📞 Call
        </button>
      </div>
    {:else if channelType === 'group-chat'}
      <!-- ZEB-360 T13: group-DM header. The Call/Join button hides once the local
           user is in this call (the in-call bar takes over); it's disabled while
           any other voice session is active (one media engine at a time). -->
      <div class="dm-header">
        <span class="dm-name">{channelName}</span>
        {#if !groupCallSelf}
          {#if groupCallActive}
            <button
              type="button"
              class="btn-call"
              data-testid="group-join-button"
              aria-label="Join call"
              disabled={groupCallBusy}
              onclick={() => onJoinGroupCall?.(channelId)}
            >
              📞 Join call
            </button>
          {:else}
            <button
              type="button"
              class="btn-call"
              data-testid="group-call-button"
              aria-label="Start call"
              disabled={groupCallBusy}
              onclick={() => onStartGroupCall?.(channelId)}
            >
              📞 Call
            </button>
          {/if}
        {/if}
      </div>
      <GroupCallBanner
        spaceId={channelId}
        invoke={groupCallInvoke ?? noopInvoke}
        {groupCall}
        busy={groupCallBusy}
        onJoin={(_callId, sid) => onJoinGroupCall?.(sid)}
      />
    {/if}
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
          {#if item.message.callEvent}
            <!-- ZEB-357: call outcomes render as a system line, not a bubble. -->
            <CallEventLine
              message={item.message}
              isSelf={item.message.sender.address === 'self' || (ownAddress !== '' && item.message.sender.address === ownAddress)}
            />
          {:else}
          <TextMessage
            message={item.message}
            {collapsed}
            {onMediaClick}
            {onAvatarClick}
            {trustService}
            {trustVersion}
            allMessages={messages}
            {onScrollToMessage}
            isSelf={item.message.sender.address === 'self' || (ownAddress !== '' && item.message.sender.address === ownAddress)}
            onDelete={onMessageDelete}
            {resolveNickname}
            {resolveCard}
            seenAt={item.message.id === seenMessageId
              ? (peerSeenAtMs ?? peerReadUpToMs)
              : undefined}
          />
          {/if}
          {#if threadMeta.has(item.message.id)}
            {@const meta = threadMeta.get(item.message.id)!}
            <ThreadIndicator
              count={meta.count}
              participants={meta.participants}
              rootId={item.message.id}
              isOpen={openThreadId === item.message.id}
              onOpen={onThreadOpen}
              onVisibilityChange={handleThreadVisibility}
              {resolveNickname}
              {resolveCard}
            />
          {/if}
        {:else}
          <QuietMessageGroup messages={item.messages} {collapsed} {onMediaClick} {onAvatarClick} {trustService} {trustVersion} {resolveNickname} {resolveCard} />
        {/if}
      {/each}
    </div>
    <ComposeBar {onSend} {channelName} {channelType} />
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

  .dm-header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 1rem;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .dm-name {
    flex: 1;
    font-weight: 600;
    font-size: 0.95rem;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .btn-call {
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-secondary);
    padding: 4px 12px;
    border-radius: 4px;
    font-size: 0.85rem;
    cursor: pointer;
    white-space: nowrap;
    flex-shrink: 0;
  }
  .btn-call:hover:not(:disabled) {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }
  .btn-call:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  /* ZEB-214: read-receipt toggle mirrors the call button; accent when on. */
  .btn-receipt {
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-secondary);
    padding: 4px 12px;
    border-radius: 4px;
    font-size: 0.85rem;
    cursor: pointer;
    white-space: nowrap;
    flex-shrink: 0;
  }
  .btn-receipt[aria-pressed='true'] {
    border-color: var(--accent);
    color: var(--accent);
  }
  .btn-receipt:hover {
    background: var(--bg-tertiary);
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
