import type { TauriAdapter } from './zenoh-service';
import type { Message, MessagePriority } from './types';
import { messages as mockMessages } from './mock-data';
import { describeCallEvent, parseCallEvent, type CallEventPayload } from './call-log';

/** Wire format for channel messages from the Rust backend. */
export interface ChannelMessageEvent {
  id: string;
  senderAddress: string;
  senderName: string;
  channel: string;
  hub: string;
  text: string;
  /** Unix timestamp in milliseconds. */
  timestamp: number;
  priority: string;
  replyTo?: string;
}

/**
 * Decode a lowercase hex string into a UTF-8 text body. The Rust event
 * loop hex-encodes DM body bytes (`hex::encode(&rm.body)`) before
 * shipping them over IPC; this is the inverse. Tolerates an empty body
 * (returns `''`); returns `''` on a malformed (odd-length / non-hex)
 * payload rather than throwing, since a DM with un-decodable body is
 * still better-rendered as an empty text bubble than a hard crash.
 */
function hexToUtf8(hex: string): string {
  if (hex.length === 0) return '';
  const pairs = hex.match(/.{2}/g);
  if (!pairs || pairs.length * 2 !== hex.length) return '';
  const bytes = new Uint8Array(pairs.length);
  for (let i = 0; i < pairs.length; i++) {
    const n = parseInt(pairs[i], 16);
    if (Number.isNaN(n)) return '';
    bytes[i] = n;
  }
  return new TextDecoder().decode(bytes);
}

/**
 * ZEB-357: when a DM carries the call-event mime type, attach the parsed
 * payload and swap the raw JSON text for the direction-aware system-line
 * label (used by previews/search and any non-system render path). A payload
 * that fails to parse leaves the message as ordinary text — the decoded JSON
 * body is an acceptable degraded rendering, never a crash.
 */
function applyCallEvent(
  msg: Message,
  mimeType: string,
  bodyText: string,
  isSelf: boolean,
): CallEventPayload | null {
  const ev = parseCallEvent(mimeType, bodyText);
  if (!ev) return null;
  msg.callEvent = ev;
  msg.text = describeCallEvent(ev, isSelf ? 'author' : 'recipient');
  return ev;
}

/**
 * Manages real-time channel messaging over Zenoh pub/sub.
 *
 * When connected, messages flow via Tauri IPC events (`message-received`).
 * When disconnected (or in browser dev mode), seeds with mock data so the
 * UI is never empty. Call `connectAdapter()` to upgrade from offline to live.
 */
export class MessageService {
  messages: Message[] = [];
  /** Called whenever the message list changes so the UI can re-render. */
  onChange?: () => void;
  /** ZEB-666: post-dedup hook for `dm-received` — fires once per unique
   *  messageCid with the raw wire payload (DmUnreadService consumes
   *  spaceId/messageCid/from/receivedAt; it does its own self-filtering
   *  via selfOwnerId). Assign BEFORE connectAdapter (an optional-chained
   *  hook is a silent no-op; nothing replays it). */
  onDmReceived?: (payload: {
    spaceId: string;
    messageCid: string;
    from: string;
    sentAt: number;
    receivedAt: number;
    body: string;
    mimeType: string;
  }) => void;
  /** Hex-encoded node address — set after Zenoh connects so we can
   *  identify self-sent messages in the echo. */
  ownAddress: string | null = null;
  /** Display name to include in outgoing messages. */
  ownDisplayName = 'You';

  /** Self author label, normalized: a whitespace-only display name (e.g. from
   *  a legacy untrimmed profile) falls back to 'You' so self-authored messages
   *  never render a blank author label. (CodeAnt, PR #180.) */
  private selfDisplayName(): string {
    return this.ownDisplayName.trim() || 'You';
  }

  private adapter: TauriAdapter | null = null;
  private connecting = false;
  private unlisteners: Array<() => void> = [];
  private seenIds = new Set<string>();
  /** IDs of constructor-seeded mock messages — used to selectively clear
   *  mocks on `connectAdapter()` while preserving any locally-created
   *  entries from offline-fallback `send()` calls during the pre-connect
   *  window (ZEB-209 bot R1). */
  private mockSeededIds = new Set<string>();
  /** Per-channel pagination cursor for `loadDmThread` — tracks the
   *  oldest `received_at.wall_ms` we've fetched so the next page-up
   *  call can ask for entries strictly older than this. */
  private dmThreadCursors = new Map<string, number>();
  /** SpaceIds we've already loaded scrollback for in this session.
   *  `loadDmThread` is called on every channel switch (see App.svelte
   *  handleNodeClick) — without this guard each switch would advance
   *  `dmThreadCursors` and prepend progressively older history pages
   *  the user didn't request (Cursor Bugbot finding on PR #81 round 2). */
  private loadedDmSpaces = new Set<string>();

  /**
   * @param opts.seedMockData Whether to seed the message list with mock
   *   messages so panes are never empty while iterating. Defaults to
   *   `import.meta.env.DEV` — true under vitest / `vite dev`, **false in a
   *   production `vite build`** (ZEB-560, mirroring NavService/VineService: a
   *   shipped/alpha build must not render fake data, and must not depend on the
   *   race-prone end-of-boot `connectAdapter()` clear). Tests pass an explicit
   *   flag to exercise both modes deterministically.
   */
  constructor(opts?: { seedMockData?: boolean }) {
    const seedMockData = opts?.seedMockData ?? import.meta.env.DEV;
    if (!seedMockData) return;
    // Seed with mock data for browser/dev mode — `connectAdapter()` clears
    // these (selectively, preserving any locally-created entries from the
    // pre-connect window) before subscribing to real events (ZEB-209).
    this.messages = [...mockMessages];
    this.mockSeededIds = new Set(mockMessages.map((m) => m.id));
    for (const m of this.messages) this.seenIds.add(m.id);
  }

  /** Connect a Tauri adapter and start listening for network messages. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter || this.connecting) return; // already wired or in-progress; prevent duplicate listeners
    this.connecting = true;

    // ── R3: Selective clear FIRST so handlers see clean state ──
    // Preserves locally-created entries from offline-fallback send() calls
    // during the pre-connect window. By the time any listener handler can
    // possibly fire, mock ids are already removed from seenIds and messages,
    // and mockSeededIds is empty — no mock-vs-real dedup collision is possible.
    // On listen failure the service is in a clean-but-unconnected state;
    // this.adapter is still null so a retry is allowed, and the selective
    // clear is idempotent (mockSeededIds is now empty, so the filter is a no-op).
    this.messages = this.messages.filter((m) => !this.mockSeededIds.has(m.id));
    for (const id of this.mockSeededIds) this.seenIds.delete(id);
    this.mockSeededIds = new Set();
    this.onChange?.();

    // ZEB-209 bot-feedback round 1: register listeners FIRST so a partial-init
    // failure doesn't wedge the service. Adapter and unlisteners are committed
    // only after all listens succeed; on failure we tear down partial work and
    // rethrow so the caller (App.svelte tryConnect) can retry.
    // ZEB-209 bot-feedback round 2: `connecting` sentinel (above) prevents a
    // concurrent caller from passing the guard while awaits are in flight.
    // ZEB-209 bot-feedback round 3: selective clear moved above listener
    // registration so handlers see clean state between successive awaits.
    const localUnlisteners: Array<() => void> = [];
    try { // outer try — releases connecting sentinel in finally on any exit path
    try {
      localUnlisteners.push(await adapter.listen(
        'message-received',
        (event) => {
          const wire = event.payload as ChannelMessageEvent;
          if (this.seenIds.has(wire.id)) return;
          this.seenIds.add(wire.id);
          const msg = this.wireToMessage(wire);
          this.messages = [...this.messages, msg];
          this.onChange?.();
        },
      ));

      // ── Phase 4 (ZEB-228) — DM lifecycle subscriptions ──────────────
      //
      // The DM transport uses a separate IPC channel from channel pub/sub.
      // `dm-received` carries hex-encoded body bytes (UTF-8 text payloads
      // for now). The lifecycle events (`dm-delivered`/`dm-expired`/
      // `dm-deleted`) correlate to a self-Message via `messageId` (hex
      // OutboxEntryId set on send). `channel` on a DM Message is the
      // SpaceId hex, matching `NavNode.id` from Task 8.

      localUnlisteners.push(await adapter.listen('dm-received', (event) => {
        const payload = event.payload as {
          spaceId: string;
          messageCid: string;
          from: string;
          sentAt: number;
          receivedAt: number;
          body: string;
          mimeType: string;
        };
        // Dedupe across reconnect/cold-start replay (messageCid is content-addressed).
        if (this.seenIds.has(payload.messageCid)) return;
        this.seenIds.add(payload.messageCid);
        // ZEB-666: unread tracking sees exactly one event per CID.
        this.onDmReceived?.(payload);

        const text = hexToUtf8(payload.body);
        const sender = (this.ownAddress && payload.from === this.ownAddress)
          ? { address: 'self', displayName: this.selfDisplayName() }
          : {
              address: payload.from,
              displayName: payload.from.slice(0, 8),
            };
        const msg: Message = {
          id: payload.messageCid,
          sender,
          text,
          timestamp: payload.sentAt,
          media: [],
          priority: 'standard',
          channel: payload.spaceId,
          // App.svelte's feed filter checks `m.hub === activeHub`. Top-level
          // DMs live with activeHub='' (findNearestFolder returns null).
          // Without `hub:''` here the filter compares '' === undefined and
          // DM messages never render. (Fix A from PR #81 review.)
          hub: '',
        };
        // ZEB-357: call-event DMs render as a system line, not a bubble.
        applyCallEvent(msg, payload.mimeType, text, sender.address === 'self');
        this.messages = [...this.messages, msg];
        this.onChange?.();
      }));

      localUnlisteners.push(await adapter.listen('dm-delivered', (event) => {
        // ZEB-231: spec-compliant payload (space_id, message_cid,
        // recipient_owner_addr). Match by (channel === spaceId &&
        // id === messageCid) — `m.id` is set to messageCid by
        // replaceOptimisticId after send_dm returns, which always
        // completes before this ack-driven event fires.
        const { spaceId, messageCid } = event.payload as {
          spaceId: string;
          messageCid: string;
          recipientOwnerAddr: string;
        };
        let changed = false;
        this.messages = this.messages.map((m) => {
          if (m.channel !== spaceId || m.id !== messageCid) return m;
          changed = true;
          return { ...m, deliveryState: 'delivered' as const };
        });
        if (changed) this.onChange?.();
      }));

      localUnlisteners.push(await adapter.listen('dm-expired', (event) => {
        // ZEB-231: spec-compliant payload (space_id, message_cid).
        // Same matcher pattern as dm-delivered.
        const { spaceId, messageCid } = event.payload as {
          spaceId: string;
          messageCid: string;
        };
        let changed = false;
        this.messages = this.messages.map((m) => {
          if (m.channel !== spaceId || m.id !== messageCid) return m;
          changed = true;
          return { ...m, deliveryState: 'expired' as const };
        });
        if (changed) this.onChange?.();
      }));

      localUnlisteners.push(await adapter.listen('dm-deleted', (event) => {
        const { spaceId, messageCid, messageId } = event.payload as {
          messageId?: string;
          spaceId: string;
          messageCid: string;
        };
        const before = this.messages.length;
        // Fix C from PR #81 review: match either id (messageCid for
        // scrollback / post-replaceOptimisticId shapes) or messageId (the
        // pre-swap optimistic placeholder still keyed by OutboxEntryId).
        // Either alone leaves orphaned bubbles in one of the two paths.
        this.messages = this.messages.filter(
          (m) =>
            !(
              m.channel === spaceId &&
              (m.id === messageCid ||
                (messageId !== undefined && m.messageId === messageId))
            ),
        );
        if (this.messages.length !== before) {
          // Drop from seenIds so a re-arrival of the same CID (e.g. peer
          // resends after we manually deleted a stuck entry) isn't deduped.
          this.seenIds.delete(messageCid);
          this.onChange?.();
        }
      }));
    } catch (err) {
      for (const fn of localUnlisteners) fn();
      // Note: mocks are already cleared and user-created entries preserved.
      // On listen failure the service is in a clean but unconnected state —
      // the App.svelte tryConnect wrapper logs. A retry is allowed
      // (this.adapter is still null), and the selective clear is idempotent
      // (mockSeededIds is now empty, so the filter is a no-op).
      throw err;
    }

    // Commit: assign adapter and register unlisteners.
    // (Selective clear was moved before listener registration per R3.)
    this.adapter = adapter;
    this.unlisteners.push(...localUnlisteners);
    } finally {
      this.connecting = false;
    }
  }

  /** Send a channel message via Tauri command. */
  async send(
    text: string,
    priority: MessagePriority,
    channel: string,
    hub: string,
    replyTo?: string,
  ): Promise<void> {
    if (this.adapter) {
      try {
        await this.adapter.invoke('send_message', {
          message: { channel, hub, text, priority, replyTo, senderName: this.ownDisplayName },
        });
        return; // Backend will echo via subscription → message-received event
      } catch (err: unknown) {
        const msg = err instanceof Error ? err.message : String(err);
        // Only fall back locally when genuinely disconnected; re-throw real errors.
        if (!msg.includes('not connected') && !msg.includes('event loop')) {
          throw err;
        }
      }
    }

    // Offline fallback: append locally so the UI stays responsive.
    const id = `msg-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
    this.seenIds.add(id);
    const msg: Message = {
      id,
      sender: { address: 'self', displayName: this.selfDisplayName() },
      text,
      timestamp: Date.now(),
      media: [],
      priority,
      replyTo,
      channel,
      hub,
    };
    this.messages = [...this.messages, msg];
    this.onChange?.();
  }

  /**
   * Phase 4 (ZEB-228) — DM optimistic-UI helpers.
   *
   * The DM send-path (App.svelte) pushes a placeholder Message into the
   * buffer immediately so the user sees their text without waiting for
   * the IPC round-trip. Once `send_dm` returns, the placeholder's id is
   * swapped for the real OutboxEntryId so subsequent dm-delivered /
   * dm-expired / dm-deleted IPCs correlate via `messageId`. On send
   * failure the placeholder is marked `deliveryState: 'failed'` rather
   * than removed — keeping a visible record lets the user see what went
   * out (and eventually retry; retry UX is a follow-up).
   */
  pushOptimistic(msg: Message): void {
    this.messages = [...this.messages, msg];
    this.seenIds.add(msg.id);
    this.onChange?.();
  }

  /**
   * Phase 4 (ZEB-228) — swap an optimistic placeholder's id for the real
   * `messageCid` (content-addressed; stable across optimistic /
   * dm-received / scrollback / dm-deleted) AND attach `messageId` (the
   * OutboxEntryId, used solely for lifecycle correlation in
   * dm-delivered / dm-expired / dm-deleted).
   *
   * Fix C from PR #81 review: previously this took just `realMessageId`
   * and used the OutboxEntryId as `id`, which broke dedup against the
   * incoming `dm-received` echo (which uses `messageCid`).
   */
  replaceOptimisticId(
    optimisticId: string,
    messageCid: string,
    messageId: string,
  ): void {
    this.messages = this.messages.map((m) =>
      m.id === optimisticId
        ? { ...m, id: messageCid, messageId }
        : m,
    );
    // Update seenIds: drop the optimistic id, add the messageCid so the
    // dm-received echo is deduped.
    this.seenIds.delete(optimisticId);
    this.seenIds.add(messageCid);
    this.onChange?.();
  }

  markFailed(optimisticId: string, _error: string): void {
    this.messages = this.messages.map((m) =>
      m.id === optimisticId ? { ...m, deliveryState: 'failed' as const } : m,
    );
    this.onChange?.();
  }

  /**
   * Phase 4 (ZEB-228) — Cold-start scrollback. Fetches decrypted DM
   * history for `spaceId` via the `read_dm_thread` IPC and merges the
   * results into the per-channel buffer in oldest-first display order.
   *
   * **First-visit-only by default.** Once a SpaceId has been loaded in
   * this session, subsequent calls are no-ops. App.svelte calls this on
   * every channel switch; without the guard each switch would advance
   * the per-channel `before_hlc` cursor and prepend progressively older
   * pages of history the user didn't ask for (Cursor Bugbot caught this
   * on PR #81 round 2). When scroll-to-top backfill arrives in a future
   * phase, it should call `loadOlderPage(spaceId)` (not yet implemented)
   * which bypasses the guard.
   *
   * Tracks a per-channel `before_hlc` cursor so a future backfill caller
   * can page backward through history. On the first call the cursor is
   * undefined → backend returns the newest page.
   *
   * For self-sent historical messages, the backend joins inbox→outbox
   * to surface the OutboxEntryId (`messageId`) and the current delivery
   * state (`'sending' | 'delivered' | 'expired'`). The frontend uses
   * `messageId` to gate the inline ⓧ delete button on stuck/expired
   * scrollback entries (Cursor Bugbot finding from PR #81 round 2).
   *
   * No-op when no adapter is connected (offline / pre-bootstrap), or
   * when the IPC returns an empty page (already at the start of the
   * thread; cursor stays put for retry).
   */
  async loadDmThread(spaceId: string, pageSize: number = 50): Promise<void> {
    if (!this.adapter) return;
    // First-visit guard: skip the IPC roundtrip + cursor advance if
    // we've already loaded this SpaceId once in this session.
    if (this.loadedDmSpaces.has(spaceId)) return;
    const cursor = this.dmThreadCursors.get(spaceId);
    type Wire = {
      messageCid: string;
      from: string;
      sentAt: number;
      receivedAt: number;
      body: string;
      mimeType: string;
      isSelfOutbound: boolean;
      /** Backend (post-b58a15b) ships per-entry delivery state for self-
       *  outbound messages: 'sending' | 'delivered' | 'expired' | 'failed'.
       *  Undefined for received entries. (Fix E from PR #81 review.) */
      deliveryState?: 'sending' | 'delivered' | 'expired' | 'failed';
      /** Self-entries with a surviving OutboxEntry surface its hex id so
       *  the frontend can gate the manual-delete button + correlate
       *  dm-delivered/expired/deleted IPC events. Undefined for received
       *  entries and for self-entries whose OutboxEntry was already GC'd
       *  post-Complete. (PR #81 round 3 fix for Cursor Bugbot.) */
      messageId?: string;
    };
    const results = await this.adapter.invoke('read_dm_thread', {
      spaceId,
      limit: pageSize,
      beforeHlc: cursor,
    }) as Wire[];
    // Mark the SpaceId as loaded BEFORE the early-return so the guard
    // also kicks in for empty-page responses (start-of-thread cold case).
    this.loadedDmSpaces.add(spaceId);
    if (results.length === 0) return;

    // Backend ships newest-first; UI displays oldest-first (scroll-to-
    // bottom contract). reverse() flips the page, then prepend so any
    // already-arrived live messages stay at the bottom.
    const newMessages: Message[] = results
      .map((r) => {
        const text = hexToUtf8(r.body);
        const sender = r.isSelfOutbound
          ? { address: 'self', displayName: this.selfDisplayName() }
          : { address: r.from, displayName: r.from.slice(0, 8) };
        const msg: Message = {
          id: r.messageCid,
          sender,
          text,
          timestamp: r.sentAt,
          media: [],
          priority: 'standard',
          channel: spaceId,
          // Fix A from PR #81 review: top-level DMs use activeHub='' in
          // App.svelte's feed filter — Messages need hub:'' to match.
          hub: '',
        };
        if (r.isSelfOutbound) {
          // Fix E from PR #81 review: consume backend's per-entry
          // deliveryState (was hardcoded 'delivered'; now reflects
          // outbox.delivery_status, e.g. 'sending' for entries not yet
          // ack'd at restart time, 'expired' for the 30-day TTL run-out).
          msg.deliveryState = r.deliveryState ?? undefined;
          // Round 3: surface the OutboxEntryId so TextMessage.canDelete
          // can render the inline ⓧ for stuck/expired scrollback entries.
          // Backend returns this only when the OutboxEntry still exists;
          // post-Complete-GC self-entries have no messageId (and no
          // delete affordance is needed since they're already delivered).
          if (r.messageId) {
            msg.messageId = r.messageId;
          }
        }
        // ZEB-357: call-event DMs render as a system line, not a bubble.
        applyCallEvent(msg, r.mimeType, text, r.isSelfOutbound);
        return msg;
      })
      .reverse();

    // Dedup: a `dm-received` IPC may have raced ahead of this fetch
    // and already added the same messageCid. Skip those — the live
    // entry is the canonical copy.
    const filtered = newMessages.filter((m) => !this.seenIds.has(m.id));
    for (const m of filtered) this.seenIds.add(m.id);

    if (filtered.length > 0) {
      this.messages = [...filtered, ...this.messages];
    }

    // Cursor for next page-up: oldest received_at from this page. The
    // backend returns newest-first, so results[results.length-1] is the
    // oldest. Stored even if we filtered everything (still want to
    // advance past this page on the next call).
    this.dmThreadCursors.set(spaceId, results[results.length - 1].receivedAt);
    this.onChange?.();
  }

  /** Convert wire format to frontend Message type. */
  private wireToMessage(wire: ChannelMessageEvent): Message {
    // Self-sent messages echo back via Zenoh — map to 'self'/ownDisplayName
    // so the rest of the UI (knownPeers filter, display name) works.
    const sender = (this.ownAddress && wire.senderAddress === this.ownAddress)
      ? { address: 'self', displayName: this.selfDisplayName() }
      : {
          address: wire.senderAddress,
          displayName: wire.senderName || wire.senderAddress.slice(0, 8),
        };

    return {
      id: wire.id,
      sender,
      text: wire.text,
      timestamp: wire.timestamp,
      media: [],
      priority: (wire.priority as MessagePriority) || 'standard',
      replyTo: wire.replyTo,
      channel: wire.channel,
      hub: wire.hub,
    };
  }

  /** Register an external unlisten handle (e.g. zenoh-status listener)
   *  so it gets cleaned up alongside the service. */
  addUnlisten(fn: () => void): void {
    this.unlisteners.push(fn);
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
  }
}
