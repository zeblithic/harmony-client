<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { ChannelMessageDto, HlcDto, ChannelAttachmentDto } from '../channel-message-service';
  import type { ChannelMessageService } from '../channel-message-service';
  import type { VotingAdapter } from '../voting-adapter';
  import type { PollMeta } from '../types/voting';
  import Avatar from './Avatar.svelte';
  import PollMessage from './PollMessage.svelte';
  import MessageAttachments from './MessageAttachments.svelte';
  import ReactionEmojiImage from './ReactionEmojiImage.svelte';
  import NamedEmojiPicker from './NamedEmojiPicker.svelte';
  import { normalizeEmoji } from '../emoji-normalize';
  import { emojiNameError, MAX_EMOJI_NAME_LEN } from '../emoji-name-validation';
  import { open } from '@tauri-apps/plugin-dialog';
  import { formatBytes, mimeCategoryIcon } from '../file-utils';
  import { buildUnifiedTimeline, type TimelineRow } from '../fork-timeline';
  import type { ResolvedCard } from '../member-card-service';
  import { nonEmpty } from '../display-label';
  import { tokenizeBody, resolveMentionLabel } from '../mention-render';
  import {
    detectMentionTrigger,
    applyMentionPick,
    filterCandidates,
    reconcileCompose,
    shiftTrackedSpans,
    type MentionCandidate,
    type TrackedMention,
  } from '../mention-compose';
  import MentionAutocomplete from './MentionAutocomplete.svelte';

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
    /** ZEB-649: the forker's stated why, quoted on the divider band. */
    forkReason = null,
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
    /** ZEB-774: roster-DTO displayName resolver — the shared-ladder rung below
     *  the live card, so an author (or inline mention) the roster already named
     *  degrades to that name rather than raw hex while their card propagates. */
    resolveRosterName,
    /** ZEB-341: open the owner_id card popover for a message author. */
    onOpenCard,
    /** ZEB-588: roster for the @-mention autocomplete — {ownerId, label} with
     *  the label pre-resolved via the shared ladder by the parent. Empty/absent
     *  → the autocomplete never opens (feature degrades to plain text). */
    mentionCandidates = [],
    composerPlaceholder,
    /** ZEB-776: true while this channel is still converging after a fresh join
     *  (known from the invite hint, not yet confirmed by a real ChannelCreate).
     *  Shows a "still syncing" banner so an empty feed doesn't read as broken. */
    channelSyncing = false,
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
    forkReason?: string | null;
    votingAdapter?: VotingAdapter;
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    resolveRosterName?: (ownerIdHex: string) => string | undefined;
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
    mentionCandidates?: MentionCandidate[];
    /** ZEB-612 S5: override for the composer placeholder (TownHallView passes
     *  "Message the room…"). Absent → the long-standing `Message #name`. */
    composerPlaceholder?: string;
    channelSyncing?: boolean;
  } = $props();

  // Local mirror of service.byChannel cache for this channel.
  let messages = $state<ChannelMessageDto[]>([]);

  // Unified timeline: snapshot (pre-fork) + live messages merged HLC-ascending.
  // Re-derived whenever messages or snapshotMessages change.
  let timeline = $derived<TimelineRow[]>(
    buildUnifiedTimeline(snapshotMessages, messages, originalCommunityName, forkedAtMs, forkReason),
  );
  let scrollAtBottom = $state(true);
  let scrollAtTop = $state(false);
  let backfillInFlight = $state(false);
  let backfillProgress = $state<{ fetched: number; totalEstimate?: number } | null>(null);
  let composeText = $state('');
  // ZEB-590: last compose text the span-shifter has seen. Kept in lockstep with
  // composeText so shiftTrackedSpans can diff each edit. A plain (non-$state)
  // shadow: it's never read in the template, only by the input handler.
  let prevComposeText = '';
  let composeError = $state<string | null>(null);
  let loadError = $state<string | null>(null);
  let posting = $state(false);
  let pendingAttachments = $state<ChannelAttachmentDto[]>([]);
  let ingesting = $state(false);
  // Bumped on every channel switch so stale in-flight attach work (dialog /
  // ingest awaits) can detect it navigated away and drop its results — mirrors
  // the `cancelled` guard the channel-switch $effect uses for listMessages.
  let attachEpoch = 0;

  // messageId whose picker popover is open, or null. Only one at a time.
  let pickerOpenFor = $state<string | null>(null);

  let scrollEl: HTMLDivElement | undefined = $state();
  let composeEl: HTMLTextAreaElement | undefined = $state();

  // ── ZEB-588: @-mention compose state ──────────────────────────────────────
  // `tracked` records each picked mention so reconcileCompose can rewrite the
  // still-intact `@Label`s into `<@ownerId>` wire tokens on send. `trigger` is
  // the active `@query` at the caret (null when not mentioning).
  let tracked = $state<TrackedMention[]>([]);
  let trigger = $state<{ query: string; atIndex: number } | null>(null);
  let acIndex = $state(0);
  const acCandidates = $derived(trigger ? filterCandidates(mentionCandidates, trigger.query) : []);
  const acOpen = $derived(acCandidates.length > 0);

  // Re-detect the trigger from the LIVE DOM value/caret (not the bound state,
  // which can lag the input event). Reset the active row on every change.
  function refreshTrigger() {
    const el = composeEl;
    if (!el) {
      trigger = null;
      return;
    }
    // ZEB-590: realign / invalidate tracked spans against the just-applied edit
    // BEFORE re-detecting the trigger, so a deleted-then-retyped label can't be
    // reclaimed by a stale pick.
    tracked = shiftTrackedSpans(prevComposeText, el.value, tracked);
    prevComposeText = el.value;
    trigger = detectMentionTrigger(el.value, el.selectionStart ?? el.value.length);
    acIndex = 0;
  }

  function pickMention(c: MentionCandidate) {
    const el = composeEl;
    if (!el || !trigger) return;
    const caret = el.selectionStart ?? el.value.length;
    const r = applyMentionPick(el.value, trigger.atIndex, caret, c);
    composeText = r.text;
    // ZEB-590: applyMentionPick is a programmatic edit (it replaces the active
    // "@query" with "@Label "), so rebase existing tracked spans across that edit
    // BEFORE appending the new pick — otherwise a mention sitting after the
    // insertion point keeps a stale `start` and fails position-anchored
    // reconcile. refreshTrigger only shifts on user `input`, which this path
    // bypasses. (Qodo PR #369.)
    tracked = [...shiftTrackedSpans(el.value, r.text, tracked), r.tracked];
    // The next user edit must diff against the post-insert text.
    prevComposeText = r.text;
    trigger = null;
    // Restore focus + caret after Svelte flushes the new bound value.
    queueMicrotask(() => {
      el.focus();
      el.setSelectionRange(r.caret, r.caret);
    });
  }
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
    // ZEB-588 (CodeRabbit): drop any in-progress @-mention picks on a channel/
    // community switch so a label picked in the previous channel can't be
    // rewritten into a stale owner id when sending here.
    // ZEB-590: prevComposeText is intentionally NOT reset here — composeText is a
    // preserved draft across switches, so the shadow stays paired with it; tracked
    // is empty here regardless, so no span can be mis-shifted.
    tracked = [];
    trigger = null;
    acIndex = 0;
    // ZEB-536: close any open reaction picker so it doesn't linger across a
    // channel switch (and its Escape/outside-click window listeners unwind).
    pickerOpenFor = null;
    composeError = null;
    loadError = null;
    reactionError = null;
    backfillProgress = null;
    pendingAttachments = [];
    ingesting = false;
    // ZEB-541: drop any in-flight custom-emoji pick target so a late ingest
    // can't react on the new channel (the epoch bump below is the hard guard).
    customEmojiFor = null;
    // Drop named-picker popover, inline-naming input, and upload-name field so
    // they don't carry stale state across a channel switch (symmetric with the
    // resets above).
    namedPickerFor = null;
    namingCid = null;
    namingValue = '';
    uploadName = '';
    attachEpoch += 1;
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

    // Pull initial page (last 100 messages — relies on the ZEB-789 default,
    // do NOT pass order: 'asc' here). Before ZEB-789 this comment was
    // accurate about the intent and wrong about the behaviour: the backend
    // bounded `limit` from the OLDEST end, so a channel past 100 messages
    // opened on its first 100 and never showed a recent one. It read as
    // healthy because the .then() scrolls to the bottom of that stale window.
    //
    // Guard against the old promise overwriting the new channel's state if
    // user switched mid-flight (Cursor Bugbot MEDIUM on PR #97 round 1).
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
    // ZEB-588: while the @-autocomplete is open, the navigation keys drive it
    // instead of the composer (Enter picks the active row rather than sending).
    if (acOpen) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        acIndex = (acIndex + 1) % acCandidates.length;
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        acIndex = (acIndex - 1 + acCandidates.length) % acCandidates.length;
        return;
      }
      if (e.key === 'Enter') {
        e.preventDefault();
        // Guard a stale index (the roster can change while the menu is open).
        const candidate = acCandidates[acIndex];
        if (candidate) pickMention(candidate);
        else acIndex = 0;
        return;
      }
      if (e.key === 'Escape') {
        e.preventDefault();
        trigger = null;
        return;
      }
    }
    if (e.key !== 'Enter') return;
    if (e.shiftKey) return; // newline; let browser handle
    e.preventDefault();
    // ZEB-588: rewrite picked @Labels into <@id> tokens + the mentions array.
    const { body, mentions } = reconcileCompose(composeText, tracked);
    const trimmedBody = body.trim();
    if ((!trimmedBody && pendingAttachments.length === 0) || posting || ingesting) return;
    posting = true;
    composeError = null;
    try {
      await channelMessageService.postMessage(
        communityId,
        channelId,
        trimmedBody,
        undefined,
        mentions,
        pendingAttachments.length > 0 ? pendingAttachments : undefined,
      );
      composeText = '';
      prevComposeText = '';
      tracked = [];
      trigger = null;
      acIndex = 0;
      pendingAttachments = [];
    } catch (e) {
      composeError = e instanceof Error ? e.message : String(e);
    } finally {
      posting = false;
    }
  }

  async function pickAttachments() {
    if (ingesting || posting) return;
    // Set the in-flight guard BEFORE awaiting the picker so rapid repeated
    // clicks can't open multiple dialogs / start concurrent ingest loops.
    ingesting = true;
    composeError = null;
    // Capture the channel generation; if the user switches channels while the
    // dialog or an ingest is in flight, drop the stale results so they can't
    // land in (or be posted to) the newly-selected channel.
    const epoch = attachEpoch;
    let selected: string | string[] | null;
    try {
      selected = await open({ multiple: true });
    } catch {
      if (epoch === attachEpoch) ingesting = false;
      return; // dialog backend error — treat like cancel
    }
    if (epoch !== attachEpoch) return; // channel switched during the dialog
    if (!selected) {
      ingesting = false;
      return;
    }
    const paths = Array.isArray(selected) ? selected : [selected];
    try {
      for (const path of paths) {
        const att = await channelMessageService.ingestArtifact(communityId, path);
        if (epoch !== attachEpoch) return; // switched mid-ingest; drop
        // CIDs are content-addressed: the same file yields the same cid. The
        // keyed {#each} on cid throws on duplicate keys, so skip an already-
        // pending cid rather than add a second chip.
        if (!pendingAttachments.some((a) => a.cid === att.cid)) {
          pendingAttachments = [...pendingAttachments, att];
        }
      }
    } catch (e) {
      if (epoch === attachEpoch) composeError = e instanceof Error ? e.message : String(e);
    } finally {
      if (epoch === attachEpoch) ingesting = false;
    }
  }

  function removePending(cid: string) {
    pendingAttachments = pendingAttachments.filter((a) => a.cid !== cid);
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

  // ZEB-432/ZEB-774 label ladder (mirrors MemberRow / FriendsPanel): local friend
  // nickname (ZEB-419) ► broadcast profile-card name (ZEB-341) ► roster-DTO
  // displayName (ZEB-774) ► truncated owner hex. Read through the resolvers so the
  // reactive nickname map / card Map / roster re-render the author label
  // automatically.
  function authorLabel(author: string): string {
    // Shared ladder (ZEB-432/ZEB-588/ZEB-774): nickname ► profile-card name ►
    // roster name ► hex.
    return resolveMentionLabel(author, resolveNickname, resolveCard, resolveRosterName);
  }

  // ZEB-536 — is the local member currently reacting with `emoji` on `msg`?
  function reactionMine(msg: ChannelMessageDto, emoji: string): boolean {
    return msg.reactions?.some((r) => r.emoji === emoji && r.mine) ?? false;
  }

  // ZEB-536 — per-(message,emoji) in-flight guard so a rapid second click does
  // not recompute `add` from the still-stale `mine` and double-send (CodeAnt
  // PR #318). Plain Set — internal click-dedup, no UI reactivity needed.
  const reactionPending = new Set<string>();

  // ZEB-536 — toggle the local member's reaction (chips + quick-react share
  // this). Fire-and-forget: the only post-await write clears the non-reactive
  // guard Set (safe after teardown).
  //
  // Surface a failed reaction in the existing `.reaction-error` banner instead
  // of swallowing it in a console.warn the tester never sees. The optimistic
  // chip update is event-driven, so on failure NOTHING renders — without this
  // the most-clicked control in the alpha just looks dead. `epoch` guards
  // against showing the error on a channel the user already switched away from
  // (mirrors the custom-emoji upload path).
  function surfaceReactionError(action: string, epoch: number, e: unknown): void {
    const msg = e instanceof Error ? e.message : String(e);
    console.warn(`${action} failed`, msg);
    if (epoch !== attachEpoch) return;
    reactionError = `Couldn't ${action}: ${msg}`;
  }

  function toggleReaction(msg: ChannelMessageDto, emoji: string): void {
    const key = `${msg.messageId}:${emoji}`;
    if (reactionPending.has(key)) return;
    const add = !reactionMine(msg, emoji);
    reactionPending.add(key);
    reactionError = null;
    const epoch = attachEpoch;
    void channelMessageService
      .reactToMessage(communityId, channelId, msg.messageId, emoji, add)
      .catch((e) => surfaceReactionError(add ? 'add that reaction' : 'remove your reaction', epoch, e))
      .finally(() => reactionPending.delete(key));
  }

  function togglePicker(messageId: string): void {
    pickerOpenFor = pickerOpenFor === messageId ? null : messageId;
  }

  // ZEB-553 a11y: roving arrow-key focus across the reaction picker's
  // role="menuitem" buttons. Guarded so it never hijacks Left/Right while the
  // user is typing in the name field or interacting with the private checkbox.
  function handlePickerKey(e: KeyboardEvent): void {
    const target = e.target as HTMLElement;
    if (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA') return;
    // The nested named-emoji popover renders its OWN role="menuitem" buttons and
    // manages its own focus. Never rove into it from the outer grid, and never
    // hijack arrows while focus is already inside it (Qodo, PR #331).
    if (target.closest('.named-popover')) return;
    if (!['ArrowRight', 'ArrowDown', 'ArrowLeft', 'ArrowUp', 'Home', 'End'].includes(e.key)) return;
    const menu = e.currentTarget as HTMLElement;
    const items = Array.from(menu.querySelectorAll<HTMLElement>('[role="menuitem"]')).filter(
      (el) => !el.closest('.named-popover'),
    );
    if (items.length === 0) return;
    const current = items.indexOf(target.closest('[role="menuitem"]') as HTMLElement);
    let next: number;
    if (e.key === 'Home') next = 0;
    else if (e.key === 'End') next = items.length - 1;
    else if (e.key === 'ArrowRight' || e.key === 'ArrowDown')
      next = current < 0 ? 0 : (current + 1) % items.length;
    else next = current < 0 ? items.length - 1 : (current - 1 + items.length) % items.length;
    e.preventDefault();
    items[next]?.focus();
  }

  // Move focus into the picker (first reaction) when it opens so arrow-key nav
  // works immediately for keyboard users (Svelte action: runs on mount).
  function autofocusFirstMenuItem(node: HTMLElement) {
    node.querySelector<HTMLElement>('[role="menuitem"]')?.focus();
  }

  // ZEB-536 — picker selection is an explicit add (spec §Design), unlike the
  // toggle semantics of chips/quick-react. Closes the popover.
  function pickFromPicker(msg: ChannelMessageDto, emoji: string): void {
    pickerOpenFor = null;
    reactionError = null;
    const epoch = attachEpoch;
    void channelMessageService
      .reactToMessage(communityId, channelId, msg.messageId, emoji, true)
      .catch((e) => surfaceReactionError('add that reaction', epoch, e));
  }

  // Task 10 — react with a stored named emoji's descriptor (CID + mime + size),
  // mirroring pickFromPicker's explicit-add semantics. Closes both popovers.
  function pickNamedEmoji(msg: ChannelMessageDto, d: { cid: string; mime: string; size: number }): void {
    namedPickerFor = null;
    pickerOpenFor = null;
    reactionError = null;
    const epoch = attachEpoch;
    void channelMessageService
      .reactToMessage(communityId, channelId, msg.messageId, '', true, d)
      .catch((e) => surfaceReactionError('add that emoji', epoch, e));
  }

  // Task 10 — open the tiny inline name input on a PUBLIC custom reaction chip.
  function startNameThis(r: NonNullable<ChannelMessageDto['reactions']>[number]): void {
    if (!r.emojiCid) return;
    namingCid = r.emojiCid;
    namingValue = '';
    namingError = null;
    namingMime = 'image/png';
    namingSize = r.emojiSize ?? 0;
  }

  function cancelNameThis(): void {
    namingCid = null;
    namingError = null;
  }

  // Task 10 — commit the typed name to the local emoji-name map. Reuses the
  // existing `reactionError` surfaced-error state on backend failure.
  async function commitNameThis(): Promise<void> {
    if (!namingCid || namingCommitting) return;
    const cid = namingCid;
    const name = namingValue.trim();
    if (!name) {
      cancelNameThis();
      return;
    }
    // Validate client-side: keep the inline input OPEN on a bad name so the
    // user's typed text isn't lost to a raw backend bounce.
    const validationErr = emojiNameError(name);
    if (validationErr) {
      namingError = validationErr;
      return;
    }
    // Close only AFTER the backend confirms, so a transient backend failure
    // keeps the typed name on screen to retry rather than dropping it.
    // `namingCommitting` guards a second Enter/✓ while the IPC is in flight
    // (CodeRabbit, PR #330).
    namingCommitting = true;
    try {
      await channelMessageService.setEmojiName(cid, name, namingMime, namingSize);
      cancelNameThis();
    } catch (e) {
      reactionError = e instanceof Error ? e.message : String(e);
    } finally {
      namingCommitting = false;
    }
  }

  // ZEB-541 — custom (CAS-backed image) emoji reaction. The hidden file input is
  // shared across messages; `customEmojiFor` records which message the in-flight
  // pick targets (cleared on channel switch via the $effect below). A surfaced
  // error message for the most recent custom-emoji pick attempt.
  let customEmojiInput: HTMLInputElement | undefined = $state();
  let customEmojiFor: string | null = null;
  let reactionError = $state<string | null>(null);

  // Foundation: custom emoji are PUBLIC by default (deduplicated, freely served).
  // This per-upload toggle opts a single emoji into the encrypted/private path.
  // Reset to public after each pick so the safe default never silently sticks.
  let customEmojiPrivate = $state(false);

  // Task 10 — named-emoji integrations. `namedPickerFor` records which message's
  // toolbar currently shows the <NamedEmojiPicker> popover. `naming*` drives the
  // tiny inline "name this" input on a PUBLIC custom reaction chip. `uploadName`
  // is the optional name typed at upload time, routed to setEmojiName after the
  // emoji is ingested + reacted with.
  let namedPickerFor = $state<string | null>(null);
  let namingCid = $state<string | null>(null);
  let namingValue = $state('');
  let namingError = $state<string | null>(null);
  // Non-reactive in-flight guard: true while a setEmojiName IPC is awaiting so a
  // repeated commit can't double-fire (we now close the input only on success).
  let namingCommitting = false;
  let namingMime = '';
  let namingSize = 0;
  let uploadName = $state('');

  // Keep the named-emoji popover aligned with the active reaction picker: if the
  // main picker closed or moved to another message, drop the named popover.
  // (Complements the channel-switch reset above, which only fires on a channel
  // change — this also covers closing/reopening the picker on the SAME message,
  // so the named popover doesn't auto-reopen.)
  $effect(() => {
    if (namedPickerFor !== null && namedPickerFor !== pickerOpenFor) {
      namedPickerFor = null;
    }
  });

  // Open the OS file picker for a custom emoji on `msg`. Mirrors the avatar
  // File-acquisition flow (ProfileEditor): a native <input type="file"> whose
  // change handler reads `files[0]` — a real web `File`, which `normalizeEmoji`
  // requires (the attachment compose path's plugin-dialog returns a path, not a
  // File, so it is NOT reused here).
  function startCustomEmojiPick(msg: ChannelMessageDto): void {
    pickerOpenFor = null;
    reactionError = null;
    customEmojiFor = msg.messageId;
    customEmojiInput?.click();
  }

  // normalize → ingest → react with the minted CID. Guard the async pipeline
  // against a channel switch with the SAME `attachEpoch` epoch the attachment
  // compose path uses: a switch bumps `attachEpoch`, so an ingest completing
  // after the user navigated away drops its result instead of reacting on the
  // wrong (community, channel).
  async function handleCustomEmojiPick(e: Event): Promise<void> {
    const input = e.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    const messageId = customEmojiFor;
    // Capture the per-upload privacy choice, then reset to the public default.
    const makePrivate = customEmojiPrivate;
    customEmojiPrivate = false;
    // Task 10 — capture + reset the optional upload-time name. Only PUBLIC emoji
    // are nameable (the name map keys by the public CID), so a private upload
    // discards any typed name.
    const nameAtUpload = uploadName.trim();
    uploadName = '';
    // Reset the input + the target so a repeated pick of the same file re-fires
    // change, and a stale target can't leak into a later pick.
    input.value = '';
    customEmojiFor = null;
    if (!file || !messageId) return;
    const epoch = attachEpoch;
    // `commId`/`chid` (not `cid`): in this codebase `cid` means a CAS content
    // identifier, and `ingestEmojiBytes` returns one as `emojiCid` below —
    // naming the community id `cid` here would shadow that meaning (Greptile).
    const commId = communityId;
    const chid = channelId;
    try {
      const bytes = await normalizeEmoji(file);
      if (epoch !== attachEpoch) return; // channel switched mid-normalize
      const { cid: emojiCid, size } = await channelMessageService.ingestEmojiBytes(commId, bytes, makePrivate);
      if (epoch !== attachEpoch) return; // channel switched mid-ingest
      await channelMessageService.reactToMessage(commId, chid, messageId, '', true, {
        cid: emojiCid,
        mime: 'image/png',
        size,
      });
      // Task 10 — if a name was typed at upload (PUBLIC emoji only), bind it now.
      // Validate client-side first so an invalid name surfaces friendly copy
      // instead of the backend's raw bounce; the emoji itself still uploaded
      // and reacted, so we only report that the NAME didn't stick.
      if (nameAtUpload && !makePrivate) {
        const nameErr = emojiNameError(nameAtUpload);
        if (nameErr) {
          reactionError = `Uploaded, but couldn't name it: ${nameErr}`;
        } else {
          await channelMessageService.setEmojiName(emojiCid, nameAtUpload, 'image/png', size);
        }
      }
    } catch (err) {
      if (epoch !== attachEpoch) return; // don't surface on a stale channel
      reactionError = err instanceof Error ? err.message : String(err);
    }
  }

  // ZEB-541 — toggle the local member's CUSTOM reaction for a chip's CID.
  // Mirrors toggleReaction (unicode) but keys/sends by `emojiCid` with the
  // descriptor the backend needs to re-authorize. Shares the reactionPending
  // in-flight guard (keyed by the CID).
  function toggleCustomReaction(msg: ChannelMessageDto, r: NonNullable<ChannelMessageDto['reactions']>[number]): void {
    if (!r.emojiCid) return;
    const key = `${msg.messageId}:${r.emojiCid}`;
    if (reactionPending.has(key)) return;
    reactionPending.add(key);
    reactionError = null;
    const add = !r.mine;
    const epoch = attachEpoch;
    void channelMessageService
      .reactToMessage(communityId, channelId, msg.messageId, '', add, {
        cid: r.emojiCid,
        mime: 'image/png',
        size: r.emojiSize ?? 0,
      })
      .catch((e) => surfaceReactionError(add ? 'add that reaction' : 'remove your reaction', epoch, e))
      .finally(() => reactionPending.delete(key));
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
  // ZEB-432/ZEB-774 author label ladder (nickname ► profile-card name ► roster
  // name ► short hex).
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

  {#if channelSyncing}
    <div class="syncing-banner" data-testid="channel-syncing-banner" role="status">
      This channel is still syncing — messages will appear shortly.
    </div>
  {/if}

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
          <span class="fork-divider-glyph" aria-hidden="true">⑂</span>
          <span class="fork-divider-text">
            <span class="fork-divider-title">Forked from {row.originalCommunityName}</span>
            {#if row.forkReason}
              <span class="fork-divider-reason">“{row.forkReason}”</span>
            {/if}
            <span class="fork-divider-meta">{new Date(row.forkedAtMs).toLocaleDateString()} · {snapshotMessages.length} message{snapshotMessages.length === 1 ? '' : 's'} carried</span>
          </span>
        </div>
      {:else if 'msg' in row}
        {@const msg = row.msg}
        <article
          class="channel-message"
          class:self={isSelf(msg.author)}
          class:pre-fork={row.isPreFork}
          class:mentions-me={msg.mentions?.includes(ownAddress)}
        >
          <div class="avatar-col">
            <Avatar address={msg.author} avatarUrl={resolveCard?.(msg.author)?.avatarUrl} size={32} />
          </div>
          <div class="content-col">
            <header class="msg-meta">
              <!-- ZEB-341/ZEB-432/ZEB-774: nickname ► profile-card name ► roster name ► hex (authorLabel). -->
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
              <p class="body">{#each tokenizeBody(bodyToText(msg.body)) as seg}{#if seg.type === 'mention'}<span
                    class="mention"
                    class:self={seg.ownerId === ownAddress}
                    data-testid="mention">@{resolveMentionLabel(seg.ownerId, resolveNickname, resolveCard, resolveRosterName)}</span>{:else}{seg.text}{/if}{/each}</p>
            {/if}
            {#if msg.attachments && msg.attachments.length > 0}
              <MessageAttachments
                {communityId}
                {channelId}
                attachments={msg.attachments}
                {channelMessageService}
              />
            {/if}
            {#if !row.isPreFork && msg.reactions && msg.reactions.length > 0}
              <div class="reactions">
                {#each msg.reactions as r (r.emojiCid ? `cid:${r.emojiCid}` : `emoji:${r.emoji}`)}
                  <button
                    type="button"
                    class="reaction-chip"
                    class:mine={r.mine}
                    title={reactorNames(r.reactors)}
                    onclick={() => (r.emojiCid ? toggleCustomReaction(msg, r) : toggleReaction(msg, r.emoji))}
                  >
                    {#if r.emojiCid}
                      <span class="reaction-emoji" aria-hidden="true">
                        <ReactionEmojiImage {communityId} {channelId} cid={r.emojiCid} {channelMessageService} />
                      </span>
                    {:else}
                      <span class="reaction-emoji" aria-hidden="true">{r.emoji}</span>
                    {/if}
                    <span class="reaction-count">{r.count}</span>
                  </button>
                  <!-- Task 10: name-this affordance — only on PUBLIC custom chips
                       (the name map keys by the public CID; encrypted emoji aren't
                       nameable). Opens a tiny inline input → setEmojiName. -->
                  {#if r.emojiCid && r.encrypted !== true}
                    {#if namingCid === r.emojiCid}
                      <input
                        class="name-this-input"
                        type="text"
                        maxlength={MAX_EMOJI_NAME_LEN}
                        aria-label="Emoji name"
                        bind:value={namingValue}
                        onkeydown={(ev) => {
                          if (ev.key === 'Enter') commitNameThis();
                          else if (ev.key === 'Escape') cancelNameThis();
                        }}
                      />
                      <button type="button" class="name-this-save" aria-label="Save emoji name" onclick={() => commitNameThis()}>✓</button>
                      {#if namingError}
                        <span class="name-this-error" role="alert">{namingError}</span>
                      {/if}
                    {:else}
                      <button type="button" class="name-this" aria-label="Name this emoji" onclick={() => startNameThis(r)}>✎</button>
                    {/if}
                  {/if}
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
              <div
                class="reaction-picker"
                role="menu"
                aria-label="Pick a reaction"
                onkeydown={handlePickerKey}
                use:autofocusFirstMenuItem
              >
                {#each PICKER_EMOJI as emoji}
                  <button
                    type="button"
                    class="picker-emoji"
                    role="menuitem"
                    aria-label={`React ${emoji}`}
                    onclick={() => pickFromPicker(msg, emoji)}
                  >{emoji}</button>
                {/each}
                <!-- ZEB-541: upload a custom (CAS-backed) image emoji. Distinct
                     class (not .picker-emoji) so it is not counted as a unicode
                     palette entry; shares the picker-emoji styling via CSS. -->
                <button
                  type="button"
                  class="picker-custom"
                  role="menuitem"
                  aria-label="Custom emoji"
                  onclick={() => startCustomEmojiPick(msg)}
                >&#x2795;</button>
                <label class="picker-private" title="Public emoji can be cached and re-shared by anyone and can't be deleted later. Check this to keep this emoji private to the community.">
                  <input
                    type="checkbox"
                    bind:checked={customEmojiPrivate}
                    aria-label="Keep custom emoji private to this community"
                    aria-describedby="picker-private-hint-{msg.messageId}"
                  />
                  <span>Keep private</span>
                </label>
                <span class="picker-private-hint" id="picker-private-hint-{msg.messageId}">Public emoji can't be deleted later.</span>
                <!-- Task 10: open the saved named-emoji popover; pick reacts on this message. -->
                <button
                  type="button"
                  class="picker-named"
                  role="menuitem"
                  aria-label="Named emoji"
                  onclick={() => (namedPickerFor = namedPickerFor === msg.messageId ? null : msg.messageId)}
                >🔖</button>
                <label class="picker-upload-name">
                  <span>Name (optional)</span>
                  <input type="text" maxlength={MAX_EMOJI_NAME_LEN} aria-label="Name new emoji" bind:value={uploadName} placeholder="catjam" />
                </label>
                {#if namedPickerFor === msg.messageId}
                  <div class="named-popover">
                    <NamedEmojiPicker {channelMessageService} onpick={(d) => pickNamedEmoji(msg, d)} />
                  </div>
                {/if}
              </div>
            {/if}
          </div>
          {/if}
        </article>
      {/if}
    {/each}
  </div>

  <!-- ZEB-541: shared hidden file input for custom-emoji reactions. A native
       <input type="file"> yields a real web File (what normalizeEmoji needs),
       mirroring the avatar pick in ProfileEditor. -->
  <input
    bind:this={customEmojiInput}
    type="file"
    accept="image/*"
    class="custom-emoji-input"
    aria-hidden="true"
    tabindex="-1"
    onchange={handleCustomEmojiPick}
  />
  {#if reactionError}
    <div class="reaction-error" role="alert">
      <span class="reaction-error-text">{reactionError}</span>
      <button
        type="button"
        class="reaction-error-dismiss"
        aria-label="Dismiss error"
        onclick={() => (reactionError = null)}
      >&times;</button>
    </div>
  {/if}

  <div class="compose">
    {#if composeError}
      <div class="compose-error" role="alert">{composeError}</div>
    {/if}
    {#if ingesting}
      <!-- Surface the in-flight ingest so pressing Enter (which no-ops while
           a file is still being read in) reads as "finishing upload", not a
           dead key. `role="status"` announces it to assistive tech. -->
      <div class="compose-hint" role="status" data-testid="compose-ingest-hint">Finishing upload…</div>
    {/if}
    {#if pendingAttachments.length > 0}
      <div class="pending-attachments">
        {#each pendingAttachments as att (att.cid)}
          <div class="pending-chip">
            <span class="att-icon" aria-hidden="true">{mimeCategoryIcon(att.mime)}</span>
            <span class="att-name" title={att.name}>{att.name}</span>
            <span class="att-size">{formatBytes(att.size)}</span>
            <button
              type="button"
              class="pending-remove"
              onclick={() => removePending(att.cid)}
              aria-label={`Remove ${att.name}`}
            >&times;</button>
          </div>
        {/each}
      </div>
    {/if}
    <div class="compose-row">
      <button
        type="button"
        class="attach-btn"
        onclick={pickAttachments}
        disabled={posting || ingesting}
        aria-label="Attach file"
        title="Attach file"
      >{ingesting ? '…' : '📎'}</button>
      <textarea
        bind:this={composeEl}
        bind:value={composeText}
        onkeydown={handleCompose}
        oninput={refreshTrigger}
        onkeyup={refreshTrigger}
        onclick={refreshTrigger}
        class="compose-input"
        placeholder={ingesting ? 'Finishing upload…' : (composerPlaceholder ?? `Message #${channelName}`)}
        rows="2"
        aria-label="Channel message"
        disabled={posting}
      ></textarea>
      {#if acOpen}
        <MentionAutocomplete candidates={acCandidates} activeIndex={acIndex} onPick={pickMention} />
      {/if}
    </div>
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
    outline: 2px solid var(--accent);
    outline-offset: 1px;
    border-radius: 2px;
  }
  .ts { color: var(--text-secondary); font-size: 0.7rem; }
  .body { margin: 2px 0 0; color: var(--text-primary); white-space: pre-wrap; word-wrap: break-word; }
  /* ZEB-588: resolved @-mention chip; `.self` = a mention of the viewer. */
  .mention {
    color: var(--accent);
    background: color-mix(in srgb, var(--accent) 15%, transparent);
    border-radius: 3px;
    padding: 0 0.15rem;
    font-weight: 500;
  }
  .mention.self {
    background: color-mix(in srgb, var(--accent) 35%, transparent);
  }
  /* ZEB-588: subtle row highlight when the viewer is mentioned in a message. */
  .channel-message.mentions-me {
    background: color-mix(in srgb, var(--accent) 8%, transparent);
    border-left: 2px solid var(--accent);
  }
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
    /* ZEB-661 (#8): round count-affordance (20px), not the 10px pill-by-coincidence. */
    border-radius: 20px;
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
  .reaction-count { color: var(--text-secondary); font-family: var(--font-mono); }
  .reaction-chip.mine .reaction-count { color: var(--text-primary); }
  /* ZEB-541: keep the inline custom-emoji image vertically centered in the chip. */
  .reaction-emoji { display: inline-flex; align-items: center; }
  /* ZEB-541: visually-hidden file input (offscreen but focusable-by-click). */
  .custom-emoji-input {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
    border: 0;
  }
  .syncing-banner {
    padding: 6px 12px;
    font-size: 0.8rem;
    color: var(--text-muted);
    background: var(--surface-raised);
    border-bottom: 1px solid var(--border);
  }
  .reaction-error {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--danger-muted);
    font-size: 0.75rem;
    padding: 4px 12px;
  }
  .reaction-error-text { flex: 1 1 auto; min-width: 0; }
  .reaction-error-dismiss {
    flex: 0 0 auto;
    background: transparent;
    border: none;
    color: inherit;
    cursor: pointer;
    font-size: 1rem;
    line-height: 1;
    padding: 0 2px;
  }
  .reaction-error-dismiss:hover { opacity: 0.7; }
  .reaction-toolbar {
    position: absolute;
    top: -10px;
    right: 14px;
    display: flex;
    align-items: center;
    gap: 2px;
    padding: 2px;
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: var(--shadow-e1);
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
  .picker-emoji,
  .picker-custom,
  .picker-named {
    background: transparent;
    border: none;
    cursor: pointer;
    font-size: 0.95rem;
    line-height: 1;
    padding: 3px 5px;
    border-radius: 5px;
  }
  .quick-react:hover,
  .picker-toggle:hover,
  .picker-emoji:hover,
  .picker-custom:hover,
  .picker-named:hover { background: var(--bg-tertiary); }
  .quick-react:focus-visible,
  .picker-toggle:focus-visible,
  .picker-emoji:focus-visible,
  .picker-custom:focus-visible,
  .picker-named:focus-visible {
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
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: var(--shadow-e2);
    z-index: 10;
  }
  /* The picker is a 5-col grid; `grid-column: 1 / -1` drops the checkbox + hint
     onto their own full-width rows below the emoji palette. */
  .picker-private {
    display: inline-flex;
    align-items: center;
    gap: 0.25rem;
    grid-column: 1 / -1;
    font-size: 0.75rem;
  }
  .picker-private-hint {
    grid-column: 1 / -1;
    font-size: 0.7rem;
    opacity: 0.6;
  }
  /* Task 10: the upload-name label + the named-emoji popover are full-width rows
     in the 5-col picker grid (same idiom as .picker-private / its hint). */
  .picker-upload-name {
    display: flex;
    flex-direction: column;
    grid-column: 1 / -1;
    font-size: 0.75em;
    gap: 0.1rem;
  }
  .named-popover {
    grid-column: 1 / -1;
    position: relative;
    z-index: 10;
  }
  /* Task 10: in-context "name this" affordance + its inline input on a chip. */
  .name-this {
    opacity: 0.4;
    cursor: pointer;
    font-size: 0.85em;
    background: transparent;
    border: none;
    color: var(--text-primary);
  }
  .name-this:hover { opacity: 1; }
  .name-this-input { width: 7rem; }
  .name-this-error { font-size: 0.7rem; color: var(--danger-muted); margin-left: 4px; }
  .compose {
    border-top: 1px solid var(--border);
    padding: 8px 16px 12px;
  }
  .compose-error {
    background: var(--bg-tertiary);
    border: 1px solid var(--danger-muted);
    color: var(--danger-muted);
    padding: 6px 8px;
    border-radius: 4px;
    font-size: 0.75rem;
    margin-bottom: 8px;
  }
  .compose-hint {
    color: var(--text-secondary);
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
    border: 1px solid var(--danger-muted);
    color: var(--danger-muted);
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
    color: var(--on-accent);
    border: none;
    padding: 4px 12px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.8rem;
    margin-left: auto;
  }
  /* ZEB-609: Commons fork-divider band (clay card). */
  .fork-divider {
    display: flex;
    align-items: center;
    gap: 10px;
    margin: 4px 16px;
    padding: 10px 14px;
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
    border-radius: 10px;
    box-shadow: var(--shadow-e1);
    user-select: none;
  }
  .fork-divider-glyph {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 26px;
    border-radius: 7px;
    background: var(--gov-clay);
    color: var(--text-bright);
    font-size: 0.9rem;
  }
  .fork-divider-text {
    display: flex;
    flex-direction: column;
    gap: 1px;
    min-width: 0;
  }
  .fork-divider-title {
    font-family: var(--font-ui);
    font-weight: 600;
    font-size: 0.8rem;
    color: var(--gov-clay-deep);
  }
  .fork-divider-meta {
    font-family: var(--font-mono);
    font-size: 0.7rem;
    color: var(--text-muted);
  }
  .fork-divider-reason {
    /* ZEB-649: the forker's quoted why — clay lead, per the Commons
       fork-accent convention (structural, not a dispute signal). */
    display: block;
    font-size: 0.8rem;
    font-style: italic;
    color: var(--gov-clay-deep);
    margin: 2px 0;
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
  .compose-row { display: flex; align-items: flex-end; gap: 8px; position: relative; }
  .attach-btn {
    flex: 0 0 auto;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    cursor: pointer;
    padding: 8px 10px;
    font-size: 1rem;
    line-height: 1;
  }
  .attach-btn:hover:not(:disabled) { background: var(--bg-hover-subtle); }
  .attach-btn:disabled { opacity: 0.6; cursor: default; }
  .pending-attachments { display: flex; flex-wrap: wrap; gap: 6px; margin-bottom: 8px; }
  .pending-chip {
    display: flex;
    align-items: center;
    gap: 6px;
    max-width: 260px;
    padding: 4px 8px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 12px;
    font-size: 0.75rem;
  }
  .pending-chip .att-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text-primary);
  }
  .pending-chip .att-size { color: var(--text-secondary); }
  .pending-remove {
    background: transparent;
    border: none;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 1rem;
    line-height: 1;
    padding: 0 2px;
  }
  .pending-remove:hover { color: var(--text-primary); }
</style>
