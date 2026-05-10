<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { ChannelMessageDto, HlcDto } from '../channel-message-service';
  import type { ChannelMessageService } from '../channel-message-service';
  import type { TrustService } from '../trust-service';
  import Avatar from './Avatar.svelte';

  let {
    communityId,
    channelId,
    channelName,
    channelMessageService,
    ownAddress,
    trustService,
    myPower,
    enableVirtualization = true,
  }: {
    communityId: string;
    channelId: string;
    channelName: string;
    channelMessageService: ChannelMessageService;
    ownAddress: string;
    trustService?: TrustService;
    myPower: number;
    /** Disable for jsdom tests where IntersectionObserver isn't reliable. */
    enableVirtualization?: boolean;
  } = $props();

  // Local mirror of service.byChannel cache for this channel.
  let messages = $state<ChannelMessageDto[]>([]);
  let scrollAtBottom = $state(true);
  let scrollAtTop = $state(false);
  let backfillInFlight = $state(false);
  let backfillProgress = $state<{ fetched: number; totalEstimate?: number } | null>(null);
  let composeText = $state('');
  let composeError = $state<string | null>(null);
  let posting = $state(false);

  let scrollEl: HTMLDivElement | undefined = $state();
  let composeEl: HTMLTextAreaElement | undefined = $state();
  let unsubChannel: (() => void) | null = null;
  let prevOnBackfillProgress: typeof channelMessageService.onBackfillProgress | undefined;
  let scrollAtTopTimer: ReturnType<typeof setTimeout> | null = null;
  const SCROLL_TOP_DEBOUNCE_MS = 250;
  const SCROLL_TOP_THRESHOLD_PX = 50;

  // Subscribe + initial list when channelId changes.
  $effect(() => {
    const cid = communityId;
    const chid = channelId;
    // Fresh local mirror per channel switch.
    messages = [];
    composeError = null;
    backfillProgress = null;

    // Tear down prior subscription before creating new one.
    if (unsubChannel) {
      unsubChannel();
      unsubChannel = null;
    }
    unsubChannel = channelMessageService.subscribeToChannel(cid, chid, (_msg) => {
      // Append in HLC-sorted insert position. Service emits AFTER its
      // internal ingest, so the cache is already sorted; we mirror by
      // re-reading rather than splicing.
      messages = channelMessageService.getMessages(cid, chid);
      // Auto-scroll to bottom on new live message IF scrollAtBottom was
      // already true. We use a microtask so the DOM update completes first.
      queueMicrotask(() => {
        if (scrollAtBottom) scrollToBottom();
      });
    });

    // Pull initial page (last 100 messages).
    void channelMessageService.listMessages(cid, chid, undefined, 100).then(() => {
      messages = channelMessageService.getMessages(cid, chid);
      queueMicrotask(scrollToBottom);
    });
  });

  onMount(() => {
    // Hook progress notifications. We chain rather than overwrite so that
    // CommunityView (which also wants progress) still gets called. Per spec
    // §8.3 the service emits per-channel progress; we filter to ours.
    prevOnBackfillProgress = channelMessageService.onBackfillProgress;
    channelMessageService.onBackfillProgress = (cid, chid, fetched, totalEstimate) => {
      prevOnBackfillProgress?.(cid, chid, fetched, totalEstimate);
      if (cid !== communityId || chid !== channelId) return;
      backfillProgress = { fetched, totalEstimate };
      if (totalEstimate !== undefined && fetched >= totalEstimate) {
        // Terminal tick: hide skeleton.
        backfillInFlight = false;
        backfillProgress = null;
      }
    };
  });

  onDestroy(() => {
    if (unsubChannel) unsubChannel();
    if (scrollAtTopTimer) clearTimeout(scrollAtTopTimer);
    // Restore prior progress callback so we don't leak this component's hook.
    channelMessageService.onBackfillProgress = prevOnBackfillProgress;
  });

  function scrollToBottom() {
    // Never auto-scroll if the user has manually navigated to the top to
    // read old messages — they would lose their reading position.
    if (!scrollEl || scrollAtTop) return;
    scrollEl.scrollTop = scrollEl.scrollHeight;
    scrollAtBottom = true;
  }

  function handleScroll() {
    if (!scrollEl) return;
    const distFromBottom = scrollEl.scrollHeight - scrollEl.scrollTop - scrollEl.clientHeight;
    scrollAtBottom = distFromBottom < 50;
    const atTop = scrollEl.scrollTop < SCROLL_TOP_THRESHOLD_PX;

    if (atTop && !scrollAtTop) {
      scrollAtTop = true;
      // Per spec §6.7: 250 ms stable-at-top + single-in-flight gate.
      if (scrollAtTopTimer) clearTimeout(scrollAtTopTimer);
      scrollAtTopTimer = setTimeout(() => {
        if (!scrollAtTop) return;
        triggerBackfill();
      }, SCROLL_TOP_DEBOUNCE_MS);
    } else if (!atTop && scrollAtTop) {
      scrollAtTop = false;
      if (scrollAtTopTimer) {
        clearTimeout(scrollAtTopTimer);
        scrollAtTopTimer = null;
      }
    }
  }

  function triggerBackfill() {
    // Use the oldest known message's HLC as `since` so the backend
    // returns events strictly older than what we already have. If no
    // messages locally yet, undefined fetches from the start.
    const oldest = messages.length > 0 ? messages[0].at : undefined;
    backfillInFlight = true;
    backfillProgress = { fetched: 0 };
    channelMessageService.requestBackfill(communityId, channelId, oldest).catch((e) => {
      // Service throws only if adapter not connected; in that case we
      // surface a transient skeleton state and clear.
      backfillInFlight = false;
      backfillProgress = null;
      console.warn('backfill request failed', e);
    });
  }

  async function handleCompose(e: KeyboardEvent) {
    if (e.key !== 'Enter') return;
    if (e.shiftKey) return; // newline; let browser handle
    e.preventDefault();
    const text = composeText.trim();
    if (!text || posting) return;
    posting = true;
    composeError = null;
    try {
      await channelMessageService.postMessage(communityId, channelId, text);
      composeText = '';
    } catch (e) {
      composeError = e instanceof Error ? e.message : String(e);
    } finally {
      posting = false;
    }
  }

  function bodyToText(body: number[]): string {
    try {
      return new TextDecoder().decode(new Uint8Array(body));
    } catch {
      return '';
    }
  }

  function isSelf(author: string): boolean {
    return author === ownAddress;
  }

  function formatTimestamp(at: HlcDto): string {
    return new Date(at.wallMs).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  }
</script>

<div class="channel-message-feed">
  <header class="channel-header">
    <span class="hash" aria-hidden="true">#</span>
    <span class="name">{channelName}</span>
  </header>

  <div
    class="messages-scroll"
    bind:this={scrollEl}
    onscroll={handleScroll}
    role="log"
    aria-live="polite"
    aria-relevant="additions"
  >
    {#if backfillInFlight}
      <div class="backfill-skeleton" role="status" aria-live="polite">
        Loading older messages…
        {#if backfillProgress?.totalEstimate}
          ({backfillProgress.fetched}/{backfillProgress.totalEstimate})
        {/if}
      </div>
    {/if}
    {#each messages as msg (msg.messageId)}
      <article class="channel-message" class:self={isSelf(msg.author)}>
        <div class="avatar-col">
          <Avatar address={msg.author} {trustService} size={32} />
        </div>
        <div class="content-col">
          <header class="msg-meta">
            <span class="author">{msg.author.slice(0, 8)}</span>
            <time class="ts" datetime={new Date(msg.at.wallMs).toISOString()}>{formatTimestamp(msg.at)}</time>
          </header>
          <p class="body">{bodyToText(msg.body)}</p>
        </div>
      </article>
    {/each}
  </div>

  <div class="compose">
    {#if composeError}
      <div class="compose-error" role="alert">{composeError}</div>
    {/if}
    <textarea
      bind:this={composeEl}
      bind:value={composeText}
      onkeydown={handleCompose}
      class="compose-input"
      placeholder={`Message #${channelName}`}
      rows="2"
      aria-label="Channel message"
      disabled={posting}
    ></textarea>
  </div>
</div>

<style>
  .channel-message-feed {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
    height: 100%;
  }
  .channel-header {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
    color: var(--text-primary);
    font-weight: 500;
  }
  .channel-header .hash { color: var(--text-secondary); }
  .channel-header .name { color: var(--text-primary); }
  .messages-scroll {
    flex: 1;
    overflow-y: auto;
    padding: 12px 0;
  }
  .backfill-skeleton {
    text-align: center;
    color: var(--text-secondary);
    font-size: 0.85rem;
    padding: 12px;
    background: var(--bg-tertiary);
    margin: 0 16px 12px;
    border-radius: 4px;
  }
  .channel-message {
    display: flex;
    gap: 10px;
    padding: 6px 16px;
  }
  .channel-message:hover { background: var(--bg-tertiary); }
  .avatar-col { flex: 0 0 auto; }
  .content-col { flex: 1; min-width: 0; }
  .msg-meta { display: flex; gap: 8px; align-items: baseline; }
  .author { color: var(--text-primary); font-weight: 500; font-size: 0.9rem; }
  .ts { color: var(--text-secondary); font-size: 0.7rem; }
  .body { margin: 2px 0 0; color: var(--text-primary); white-space: pre-wrap; word-wrap: break-word; }
  .compose {
    border-top: 1px solid var(--border);
    padding: 8px 16px 12px;
  }
  .compose-error {
    background: var(--bg-tertiary);
    border: 1px solid #d83c3e;
    color: #d83c3e;
    padding: 6px 8px;
    border-radius: 4px;
    font-size: 0.75rem;
    margin-bottom: 8px;
  }
  .compose-input {
    width: 100%;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    padding: 8px 10px;
    font-size: 0.9rem;
    font-family: inherit;
    resize: vertical;
    box-sizing: border-box;
  }
  .compose-input:focus { outline: 2px solid var(--accent); outline-offset: -1px; }
  .compose-input:disabled { opacity: 0.6; }
</style>
