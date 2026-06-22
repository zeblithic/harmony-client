<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { ChannelMessageDto, HlcDto } from '../channel-message-service';
  import type { ChannelMessageService } from '../channel-message-service';
  import type { VotingAdapter } from '../voting-adapter';
  import type { PollMeta } from '../types/voting';
  import Avatar from './Avatar.svelte';
  import PollMessage from './PollMessage.svelte';
  import { buildUnifiedTimeline, type TimelineRow } from '../fork-timeline';
  import type { ResolvedCard } from '../member-card-service';
  import { nonEmpty } from '../display-label';

  let {
    communityId,
    channelId,
    channelName,
    channelMessageService,
    ownAddress,
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
    /** ZEB-432: optional local friend-nickname resolver (ZEB-419). Takes
     *  precedence over the broadcast profile-card name on message authors,
     *  matching the members roster and the Friends panel. */
    resolveNickname,
    /** ZEB-341: open the owner_id card popover for a message author. */
    onOpenCard,
  }: {
    communityId: string;
    channelId: string;
    channelName: string;
    channelMessageService: ChannelMessageService;
    ownAddress: string;
    myPower: number;
    /** Disable for jsdom tests where IntersectionObserver isn't reliable. */
    enableVirtualization?: boolean;
    snapshotMessages?: ChannelMessageDto[];
    originalCommunityName?: string;
    forkedAtMs?: number;
    votingAdapter?: VotingAdapter;
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    onOpenCard?: (
      payload: {
        ownerIdHex: string;
        displayName: string;
        statusText: string;
        power?: number;
        membershipStatus?: string;
        avatarUrl?: string;
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

  // messageId whose picker popover is open, or null. Only one at a time.
  let pickerOpenFor = $state<string | null>(null);

  let scrollEl: HTMLDivElement | undefined = $state();
  let composeEl: HTMLTextAreaElement | undefined = $state();
  let unsubChannel: (() => void) | null = null;
  let prevOnBackfillProgress: typeof channelMessageService.onBackfillProgress | undefined;
  let scrollAtTopTimer: ReturnType<typeof setTimeout> | null = null;
  const SCROLL_TOP_DEBOUNCE_MS = 250;
  const SCROLL_TOP_THRESHOLD_PX = 50;

  // ZEB-536 reaction palette (v1). The grid is a const array — trim toward
  // quick-react-only later if it feels bloated (spec §Design).
  const QUICK_REACTIONS = ['👍', '👎'];
  const PICKER_EMOJI = ['👍', '👎', '✅', '❌', '👀', '🎉', '🙏', '🚀', '❤️', '😄'];

  // Subscribe + initial list when channelId changes.
  $effect(() => {
    const cid = communityId;
    const chid = channelId;
    let cancelled = false;
    // ZEB-536: keep selfOwnerId current for live `mine` on reaction events.
    // Set here (not just onMount) so it survives a service destroy()/reuse and
    // channel switches; safe because there is a single local owner and
    // list_channel_messages carries authoritative `mine` on (re)open.
    channelMessageService.selfOwnerId = ownAddress;
    // Fresh local mirror per channel switch.
    messages = [];
    // ZEB-536: close any open reaction picker so it doesn't linger across a
    // channel switch (and its Escape/outside-click window listeners unwind).
    pickerOpenFor = null;
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

  // ZEB-432 label ladder (mirrors MemberRow / FriendsPanel): local friend
  // nickname (ZEB-419) ► broadcast profile-card name (ZEB-341) ► truncated owner
  // hex. Read through both resolvers so the reactive nickname map / card Map
  // re-render the author label automatically.
  function authorLabel(author: string): string {
    return (
      nonEmpty(resolveNickname?.(author)) ??
      nonEmpty(resolveCard?.(author)?.displayName) ??
      author.slice(0, 8)
    );
  }

  // ZEB-536 — is the local member currently reacting with `emoji` on `msg`?
  function reactionMine(msg: ChannelMessageDto, emoji: string): boolean {
    return msg.reactions?.some((r) => r.emoji === emoji && r.mine) ?? false;
  }

  // ZEB-536 — toggle the local member's reaction (chips + quick-react share
  // this). Fire-and-forget: no component-state write after the await, so no
  // teardown guard is needed; failures are logged, not surfaced (the chip
  // self-heals from the authoritative event / next list).
  function toggleReaction(msg: ChannelMessageDto, emoji: string): void {
    const add = !reactionMine(msg, emoji);
    void channelMessageService
      .reactToMessage(communityId, channelId, msg.messageId, emoji, add)
      .catch((e) => console.warn('reaction toggle failed', e instanceof Error ? e.message : String(e)));
  }

  function togglePicker(messageId: string): void {
    pickerOpenFor = pickerOpenFor === messageId ? null : messageId;
  }

  // ZEB-536 — picker selection is an explicit add (spec §Design), unlike the
  // toggle semantics of chips/quick-react. Closes the popover.
  function pickFromPicker(msg: ChannelMessageDto, emoji: string): void {
    pickerOpenFor = null;
    void channelMessageService
      .reactToMessage(communityId, channelId, msg.messageId, emoji, true)
      .catch((e) => console.warn('reaction pick failed', e instanceof Error ? e.message : String(e)));
  }

  // Close the open reaction picker on Escape or an outside click. Listeners
  // are scoped to "a picker is open" and cleaned up on close/teardown. The
  // click that opened the picker targets a node inside `.reaction-toolbar`,
  // so it does not self-close.
  $effect(() => {
    if (pickerOpenFor === null) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') pickerOpenFor = null;
    };
    const onDocClick = (e: MouseEvent) => {
      const t = e.target as HTMLElement | null;
      if (!t) {
        pickerOpenFor = null;
        return;
      }
      // Every message renders its own .reaction-toolbar, so matching *any*
      // toolbar would leave the picker open when another message's toolbar is
      // clicked. Keep it open only for clicks inside the OPEN picker's own
      // toolbar (Greptile PR #316).
      const toolbar = t.closest<HTMLElement>('.reaction-toolbar');
      if (!toolbar || toolbar.dataset.messageId !== pickerOpenFor) {
        pickerOpenFor = null;
      }
    };
    window.addEventListener('keydown', onKey);
    window.addEventListener('click', onDocClick);
    return () => {
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('click', onDocClick);
    };
  });

  // ZEB-536 — comma-joined reactor labels for a chip tooltip, reusing the
  // ZEB-432 author label ladder (nickname ► profile-card name ► short hex).
  function reactorNames(reactors: string[]): string {
    return reactors.map((addr) => authorLabel(addr)).join(', ');
  }

  function handleAuthorClick(author: string, ev: MouseEvent) {
    // Resolve once — a single map lookup and a single reactive cardVersion read.
    const card = resolveCard?.(author);
    onOpenCard?.(
      {
        // ZEB-432 (PR #240 review): the identity drill-down popover shows the
        // SIGNED profile-card name, never the local nickname — a private label
        // must not masquerade as the cryptographic identity (mirrors
        // FriendsPanel). The inline author label stays nickname-first.
        ownerIdHex: author,
        displayName: nonEmpty(card?.displayName) ?? author.slice(0, 8),
        statusText: card?.statusText ?? '',
        avatarUrl: card?.avatarUrl,
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
            <Avatar address={msg.author} avatarUrl={resolveCard?.(msg.author)?.avatarUrl} size={32} />
          </div>
          <div class="content-col">
            <header class="msg-meta">
              <!-- ZEB-341/ZEB-432: nickname ► profile-card name ► hex (authorLabel). -->
              {#if onOpenCard}
                <button
                  type="button"
                  class="author author-btn"
                  onclick={(e) => handleAuthorClick(msg.author, e)}
                >{authorLabel(msg.author)}</button>
              {:else}
                <span class="author">{authorLabel(msg.author)}</span>
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
            {#if !row.isPreFork && msg.reactions && msg.reactions.length > 0}
              <div class="reactions">
                {#each msg.reactions as r (r.emoji)}
                  <button
                    type="button"
                    class="reaction-chip"
                    class:mine={r.mine}
                    title={reactorNames(r.reactors)}
                    onclick={() => toggleReaction(msg, r.emoji)}
                  >
                    <span class="reaction-emoji" aria-hidden="true">{r.emoji}</span>
                    <span class="reaction-count">{r.count}</span>
                  </button>
                {/each}
              </div>
            {/if}
          </div>
          {#if !row.isPreFork}
          <div class="reaction-toolbar" role="group" aria-label="Add reaction" data-message-id={msg.messageId}>
            {#each QUICK_REACTIONS as emoji}
              <button
                type="button"
                class="quick-react"
                class:active={reactionMine(msg, emoji)}
                aria-label={`React ${emoji}`}
                aria-pressed={reactionMine(msg, emoji)}
                onclick={() => toggleReaction(msg, emoji)}
              >{emoji}</button>
            {/each}
            <button
              type="button"
              class="picker-toggle"
              aria-label="More reactions"
              aria-haspopup="menu"
              aria-expanded={pickerOpenFor === msg.messageId}
              onclick={() => togglePicker(msg.messageId)}
            >😊</button>
            {#if pickerOpenFor === msg.messageId}
              <div class="reaction-picker" role="menu" aria-label="Pick a reaction">
                {#each PICKER_EMOJI as emoji}
                  <button
                    type="button"
                    class="picker-emoji"
                    role="menuitem"
                    aria-label={`React ${emoji}`}
                    onclick={() => pickFromPicker(msg, emoji)}
                  >{emoji}</button>
                {/each}
              </div>
            {/if}
          </div>
          {/if}
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
    position: relative;
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
  .reactions {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-top: 4px;
  }
  .reaction-chip {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 1px 8px;
    font-size: 0.8rem;
    line-height: 1.4;
    color: var(--text-primary);
    cursor: pointer;
  }
  .reaction-chip:hover { background: var(--bg-tertiary); }
  .reaction-chip.mine {
    border-color: var(--accent);
    background: color-mix(in srgb, var(--accent) 18%, transparent);
  }
  .reaction-chip:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .reaction-count { color: var(--text-secondary); }
  .reaction-chip.mine .reaction-count { color: var(--text-primary); }
  .reaction-toolbar {
    position: absolute;
    top: -10px;
    right: 14px;
    display: flex;
    align-items: center;
    gap: 2px;
    padding: 2px;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 6px;
    box-shadow: 0 1px 4px rgba(0, 0, 0, 0.3);
    opacity: 0;
    pointer-events: none;
    transition: opacity 0.08s ease;
  }
  .channel-message:hover .reaction-toolbar,
  .reaction-toolbar:focus-within {
    opacity: 1;
    pointer-events: auto;
  }
  .quick-react,
  .picker-toggle,
  .picker-emoji {
    background: transparent;
    border: none;
    cursor: pointer;
    font-size: 0.95rem;
    line-height: 1;
    padding: 3px 5px;
    border-radius: 4px;
  }
  .quick-react:hover,
  .picker-toggle:hover,
  .picker-emoji:hover { background: var(--bg-tertiary); }
  .quick-react:focus-visible,
  .picker-toggle:focus-visible,
  .picker-emoji:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .quick-react.active {
    background: color-mix(in srgb, var(--accent) 22%, transparent);
  }
  .reaction-picker {
    position: absolute;
    top: 100%;
    right: 0;
    margin-top: 4px;
    display: grid;
    grid-template-columns: repeat(5, 1fr);
    gap: 2px;
    padding: 4px;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 6px;
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.4);
    z-index: 10;
  }
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
