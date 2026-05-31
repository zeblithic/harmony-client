<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { ChannelMessageDto, HlcDto } from '../channel-message-service';
  import type { ChannelMessageService } from '../channel-message-service';
  import type { TrustService } from '../trust-service';
  import type { VotingAdapter } from '../voting-adapter';
  import type { PollMeta } from '../types/voting';
  import Avatar from './Avatar.svelte';
  import PollMessage from './PollMessage.svelte';
  import { buildUnifiedTimeline, type TimelineRow } from '../fork-timeline';
  import type { ResolvedCard } from '../member-card-service';

  let {
    communityId,
    channelId,
    channelName,
    channelMessageService,
    ownAddress,
    trustService,
    myPower,
    enableVirtualization = true,
    /** ZEB-285 Task 11: pre-fork snapshot messages for this channel (HLC-ascending). */
    snapshotMessages = [],
    /** ZEB-285 Task 11: display name of the original community at fork time. */
    originalCommunityName = '',
    /** ZEB-285 Task 11: wall-clock ms of the fork point (for the divider label). */
    forkedAtMs = 0,
    /**
     * ZEB-291 Phase 1.5 chat dispatch — optional voting adapter. When
     * provided, poll-kind messages (`msg.kind === 'poll'`) render an
     * inline `<PollMessage>` card; otherwise they fall through to the
     * text rendering as a degraded but readable fallback. The adapter
     * is also used to pre-fetch poll metas via `listActivePolls` so
     * the card renders instantly without a per-message round trip.
     */
    votingAdapter,
    /** ZEB-341: optional card resolver for author display names. */
    resolveCard,
    /** ZEB-341: open the owner_id card popover for a message author. */
    onOpenCard,
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
    snapshotMessages?: ChannelMessageDto[];
    originalCommunityName?: string;
    forkedAtMs?: number;
    votingAdapter?: VotingAdapter;
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    onOpenCard?: (
      payload: {
        ownerIdHex: string;
        displayName: string;
        statusText: string;
        power?: number;
        membershipStatus?: string;
      },
      ev: MouseEvent,
    ) => void;
  } = $props();

  // Local mirror of service.byChannel cache for this channel.
  let messages = $state<ChannelMessageDto[]>([]);

  // Unified timeline: snapshot (pre-fork) + live messages merged HLC-ascending.
  // Re-derived whenever messages or snapshotMessages change.
  let timeline = $derived<TimelineRow[]>(
    buildUnifiedTimeline(snapshotMessages, messages, originalCommunityName, forkedAtMs),
  );
  let scrollAtBottom = $state(true);
  let scrollAtTop = $state(false);
  let backfillInFlight = $state(false);
  let backfillProgress = $state<{ fetched: number; totalEstimate?: number } | null>(null);
  let composeText = $state('');
  let composeError = $state<string | null>(null);
  let loadError = $state<string | null>(null);
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
    let cancelled = false;
    // Fresh local mirror per channel switch.
    messages = [];
    composeError = null;
    loadError = null;
    backfillProgress = null;
    // Phase 4 round-1 fixup: also reset scroll/backfill state to avoid
    // bleed-over between channels (Qodo PR #97 finding).
    backfillInFlight = false;
    scrollAtBottom = true;
    scrollAtTop = false;
    if (scrollAtTopTimer) {
      clearTimeout(scrollAtTopTimer);
      scrollAtTopTimer = null;
    }

    // Tear down prior subscription before creating new one.
    if (unsubChannel) {
      unsubChannel();
      unsubChannel = null;
    }
    unsubChannel = channelMessageService.subscribeToChannel(cid, chid, (_msg) => {
      if (cancelled) return;
      // Append in HLC-sorted insert position. Service emits AFTER its
      // internal ingest, so the cache is already sorted; we mirror by
      // re-reading rather than splicing.
      messages = channelMessageService.getMessages(cid, chid);
      // Auto-scroll to bottom on new live message IF scrollAtBottom was
      // already true. We use a microtask so the DOM update completes first.
      queueMicrotask(() => {
        if (cancelled) return;
        if (scrollAtBottom) scrollToBottom();
      });
    });

    // Pull initial page (last 100 messages). Guard against the old promise
    // overwriting the new channel's state if user switched mid-flight
    // (Cursor Bugbot MEDIUM on PR #97 round 1).
    void channelMessageService.listMessages(cid, chid, undefined, 100)
      .then(() => {
        if (cancelled) return;
        messages = channelMessageService.getMessages(cid, chid);
        queueMicrotask(() => {
          if (cancelled) return;
          scrollToBottom();
        });
      })
      .catch((err) => {
        if (cancelled) return;
        loadError = err instanceof Error ? err.message : String(err);
      });

    return () => {
      cancelled = true;
    };
  });

  // ZEB-291 Phase 1.5 chat dispatch — poll meta cache keyed by hex
  // poll_id (matches `ChannelMessageDto.pollId` from the Rust IPC
  // boundary). Pre-fetched via `listActivePolls` so a poll-kind
  // message in the feed can render `<PollMessage>` immediately
  // without a per-message `getPoll` round trip.
  //
  // Re-runs when communityId or votingAdapter changes (e.g. a parent
  // swap during testing or a re-connect). Cancelled flag protects
  // against late completions overwriting fresh state.
  let pollMetaCache = $state<Map<string, PollMeta>>(new Map());

  $effect(() => {
    if (!votingAdapter) {
      pollMetaCache = new Map();
      return;
    }
    const cid = communityId;
    let cancelled = false;
    void (async () => {
      try {
        const polls = await votingAdapter.listActivePolls(cid);
        if (cancelled) return;
        const m = new Map<string, PollMeta>();
        for (const p of polls) {
          // poll_id is `number[]` over Tauri JSON IPC (see PollIdHex
          // JSDoc in types/voting.ts); convert to lowercase hex to
          // match the `kind=poll` discriminator emitted by the Rust
          // detect_poll_kind helper.
          const hex = (p.poll_id as number[])
            .map((b) => b.toString(16).padStart(2, '0'))
            .join('');
          m.set(hex, p);
        }
        pollMetaCache = m;
      } catch {
        // Non-fatal: chat feed still renders text messages even if
        // poll meta pre-fetch fails (e.g. node not running). Poll
        // bodies will just show a "Loading poll…" placeholder.
      }
    })();
    return () => {
      cancelled = true;
    };
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

  function retryLoad() {
    loadError = null;
    void channelMessageService.listMessages(communityId, channelId, undefined, 100)
      .then(() => {
        messages = channelMessageService.getMessages(communityId, channelId);
        queueMicrotask(scrollToBottom);
      })
      .catch((err) => {
        loadError = err instanceof Error ? err.message : String(err);
      });
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

  function handleAuthorClick(author: string, ev: MouseEvent) {
    onOpenCard?.(
      {
        ownerIdHex: author,
        displayName: resolveCard?.(author)?.displayName ?? author.slice(0, 8),
        statusText: resolveCard?.(author)?.statusText ?? '',
        // No power known for message authors → role line omitted.
      },
      ev,
    );
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
    {#if loadError}
      <div class="load-error" role="alert">
        Couldn't load messages: {loadError}
        <button type="button" class="retry-btn" onclick={retryLoad}>Retry</button>
      </div>
    {/if}
    {#each timeline as row}
      {#if 'kind' in row && row.kind === 'fork-divider'}
        <div
          class="fork-divider"
          role="separator"
          aria-label="Forked from {row.originalCommunityName}"
        >
          ───── Forked from {row.originalCommunityName} on {new Date(row.forkedAtMs).toLocaleDateString()} ─────
        </div>
      {:else if 'msg' in row}
        {@const msg = row.msg}
        <article
          class="channel-message"
          class:self={isSelf(msg.author)}
          class:pre-fork={row.isPreFork}
        >
          <div class="avatar-col">
            <Avatar address={msg.author} {trustService} size={32} />
          </div>
          <div class="content-col">
            <header class="msg-meta">
              <!-- ZEB-341 Task 1: show resolved display name when available (self-first, offline). -->
              {#if onOpenCard}
                <button
                  type="button"
                  class="author author-btn"
                  onclick={(e) => handleAuthorClick(msg.author, e)}
                >{resolveCard?.(msg.author)?.displayName ?? msg.author.slice(0, 8)}</button>
              {:else}
                <span class="author">{resolveCard?.(msg.author)?.displayName ?? msg.author.slice(0, 8)}</span>
              {/if}
              <time class="ts" datetime={new Date(msg.at.wallMs).toISOString()}>{formatTimestamp(msg.at)}</time>
              {#if row.isPreFork}
                <span class="pre-fork-badge" aria-label="From original community">from {originalCommunityName}</span>
              {/if}
            </header>
            {#if msg.kind === 'poll' && msg.pollId && votingAdapter}
              {@const pollMeta = pollMetaCache.get(msg.pollId)}
              {#if pollMeta}
                <PollMessage
                  pollId={pollMeta.poll_id}
                  meta={pollMeta}
                  adapter={votingAdapter}
                />
              {:else}
                <p class="poll-loading">Loading poll…</p>
              {/if}
            {:else}
              <p class="body">{bodyToText(msg.body)}</p>
            {/if}
          </div>
        </article>
      {/if}
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
  .author-btn {
    background: transparent;
    border: none;
    padding: 0;
    margin: 0;
    cursor: pointer;
    font: inherit;
    font-weight: 500;
    color: var(--text-primary);
  }
  .author-btn:hover { text-decoration: underline; }
  .author-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
    border-radius: 2px;
  }
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
  .load-error {
    background: var(--bg-tertiary);
    border: 1px solid #d83c3e;
    color: #d83c3e;
    padding: 10px 14px;
    border-radius: 4px;
    margin: 8px 16px;
    font-size: 0.85rem;
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .retry-btn {
    background: var(--accent);
    color: var(--text-primary);
    border: none;
    padding: 4px 12px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.8rem;
    margin-left: auto;
  }
  /* ZEB-285 Task 11: fork-point divider */
  .fork-divider {
    text-align: center;
    font-size: 0.75rem;
    color: var(--text-secondary);
    padding: 10px 16px;
    margin: 4px 16px;
    border-top: 1px solid var(--border);
    border-bottom: 1px solid var(--border);
    user-select: none;
  }
  /* Pre-fork messages rendered with muted opacity to visually distinguish
   * them from live post-fork messages per spec §6.4. */
  .channel-message.pre-fork {
    opacity: 0.65;
  }
  .poll-loading {
    margin: 4px 0 0;
    color: var(--text-secondary);
    font-size: 0.85rem;
    font-style: italic;
  }
  .pre-fork-badge {
    font-size: 0.68rem;
    color: var(--text-secondary);
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 3px;
    padding: 0 4px;
    margin-left: 4px;
    white-space: nowrap;
  }
</style>
