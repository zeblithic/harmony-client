import type { TauriAdapter } from './zenoh-service';
import { compareHlc } from './hlc';

export interface HlcDto {
  wallMs: number;
  logical: number;
  deviceId: string;
}

export interface ChannelMessageDto {
  messageId: string;
  communityId: string;
  channelId: string;
  author: string;
  at: HlcDto;
  body: number[];
  replyTo?: string;
  /**
   * ZEB-291 Phase 1.5 chat dispatch — message kind discriminator.
   * Defaults to 'text' for backward compat. Set to 'poll' by the Rust
   * IPC boundary when the body matches the poll-message convention
   * (`0x00` magic byte + 64 ASCII hex chars = the poll_id).
   */
  kind?: 'text' | 'poll';
  /**
   * ZEB-291 Phase 1.5 — present iff `kind === 'poll'`. Hex 64-char
   * (32 bytes) PollId, extracted from the body convention by the Rust
   * IPC boundary. UI dispatches to `<PollMessage>` keyed on this.
   */
  pollId?: string;
  /**
   * ZEB-536 — per-emoji reaction summaries. Optional: populated by the
   * Rust IPC boundary; consumed by the reactions UI (Spec 2).
   *
   * ZEB-541 — a custom (CAS-backed image) reaction carries `emojiCid`
   * (hex ContentId) + `emojiSize` (plaintext byte length, advisory) and an
   * empty-string `emoji`. Both fields are absent for unicode reactions.
   */
  reactions?: {
    emoji: string;
    count: number;
    mine: boolean;
    reactors: string[];
    emojiCid?: string;
    emojiSize?: number;
    /** Present iff custom: whether the emoji CID is encrypted. The UI hides the
     *  "name this emoji" affordance on encrypted chips. */
    encrypted?: boolean;
  }[];
  /**
   * ZEB-534: owner-ids (hex) this message addresses, or absent if none.
   * Recipients derive "mentions me" as `selfOwnerHex` ∈ mentions. GUI
   * render/notify is a follow-up; this field just carries the data.
   */
  mentions?: string[];
  /** CAS artifacts this message references; absent if none. */
  attachments?: ChannelAttachmentDto[];
}

export interface ChannelAttachmentDto {
  cid: string;       // hex-encoded 32-byte ContentId
  mime: string;
  name: string;
  size: number;
  encrypted: boolean;
}

export interface EmojiNameDto {
  cid: string;
  name: string;
  mime: string;
  size: number;
}

interface ChannelMessageReceivedPayload {
  communityId: string;
  channelId: string;
  message: ChannelMessageDto;
}

interface ChannelBackfillProgressPayload {
  communityId: string;
  channelId: string;
  fetched: number;
  totalEstimate?: number;
}

/**
 * ZEB-536 Spec 2 — payload of the `channel-reaction-received` event.
 * `reactor` and the message's reaction `reactors[]` are owner-id hex
 * (same space as `ChannelMessageDto.author`). `at` is ignored in v1
 * (no frontend LWW-by-HLC; list reseeds on channel open).
 */
interface ChannelReactionReceivedPayload {
  communityId: string;
  channelId: string;
  messageId: string;
  reactor: string;
  emoji: string;
  add: boolean;
  at: HlcDto;
  /**
   * ZEB-541 — present iff this is a custom (CAS-backed image) reaction. The CID
   * (hex ContentId) is materialized onto the chip entry so the feed can resolve
   * + render the image; `emojiSize` is advisory (plaintext byte length).
   */
  emojiCid?: string;
  emojiSize?: number;
  encrypted?: boolean;
}

function chKey(communityId: string, channelId: string): string {
  return `${communityId}:${channelId}`;
}

/**
 * Per-channel message cache + subscribe API + IPC facade for the three
 * channel-message IPCs shipped in ZEB-270 Phase 3
 * (`post_channel_message` / `list_channel_messages` /
 * `request_channel_backfill`). Mirrors MessageService (the DM service)
 * shape — connectAdapter installs listeners, method facades validate
 * args + dispatch, destroy() unwinds. Single-in-flight backfill gate is
 * keyed per (communityId, channelId) per spec §6.7.
 *
 * Per-channel cache is sorted by HLC ascending so insert-on-event is
 * cheap and consumers can render oldest-at-top without re-sorting.
 * Dedupe is keyed by messageId (Phase 3 ChannelLogReplayTracker handles
 * the protocol-level dedup; this is a defense-in-depth at the UI layer
 * for cases like backfill-while-live overlapping the same event id).
 */
export class ChannelMessageService {
  /** Called whenever any channel sees a new message (live or backfilled). */
  onMessage?: (communityId: string, channelId: string, message: ChannelMessageDto) => void;
  /** Called for each channel-backfill-progress tick. */
  onBackfillProgress?: (
    communityId: string,
    channelId: string,
    fetched: number,
    totalEstimate?: number,
  ) => void;
  /**
   * Owner-id hex of the local member, used to compute `mine` for live
   * reaction events. Set by the feed from its `ownAddress` prop.
   *
   * Normalized so an empty/whitespace string (the feed passes `''` while
   * identity is still loading) is treated as "unknown" (`null`), NOT as a
   * real owner id. While unknown, `applyReaction` preserves the authoritative
   * `mine` that `list_channel_messages` supplied rather than clobbering it to
   * false (Qodo PR #318). When it transitions from unknown to a real id, the
   * cached reaction `mine` flags self-heal so the UI needs no reload.
   */
  private _selfOwnerId: string | null = null;

  get selfOwnerId(): string | null {
    return this._selfOwnerId;
  }

  set selfOwnerId(value: string | null) {
    const next = value && value.trim().length > 0 ? value : null;
    if (next === this._selfOwnerId) return;
    this._selfOwnerId = next;
    if (next) this.recomputeMineFlags(next);
  }

  /** Listeners notified when the backend emits `emoji-names-changed` (the local
   *  emoji-name map was set/cleared, possibly on another device). A registry
   *  (not a single slot) so multiple UIs can each subscribe without stomping —
   *  mirrors FriendService.friendsChangedListeners. */
  private emojiNamesChangedListeners = new Set<() => void>();

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];
  private byChannel = new Map<string, ChannelMessageDto[]>();
  private subscribers = new Map<string, Set<(msg: ChannelMessageDto) => void>>();
  private inFlightBackfill = new Set<string>();
  private seenIds = new Map<string, Set<string>>();

  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    const unlistenMsg = await adapter.listen('channel-message-received', (event) => {
      const p = event.payload as ChannelMessageReceivedPayload;
      this.ingest(p.communityId, p.channelId, p.message);
    });
    this.unlisteners.push(unlistenMsg);

    const unlistenProgress = await adapter.listen('channel-backfill-progress', (event) => {
      const p = event.payload as ChannelBackfillProgressPayload;
      // Terminal tick (fetched == totalEstimate) releases the in-flight
      // backfill gate so subsequent scroll-trigger fires can re-request.
      // Non-terminal ticks just notify the UI for skeleton updates.
      if (p.totalEstimate !== undefined && p.fetched >= p.totalEstimate) {
        this.inFlightBackfill.delete(chKey(p.communityId, p.channelId));
      }
      this.onBackfillProgress?.(p.communityId, p.channelId, p.fetched, p.totalEstimate);
    });
    this.unlisteners.push(unlistenProgress);

    const unlistenReaction = await adapter.listen('channel-reaction-received', (event) => {
      const p = event.payload as ChannelReactionReceivedPayload;
      this.applyReaction(p);
    });
    this.unlisteners.push(unlistenReaction);

    const unlistenEmojiNames = await adapter.listen('emoji-names-changed', () => {
      // Snapshot before iterating so a listener that unsubscribes itself
      // during notification doesn't mutate the live set mid-loop.
      for (const cb of [...this.emojiNamesChangedListeners]) cb();
    });
    this.unlisteners.push(unlistenEmojiNames);
  }

  /** Post a message. Returns the engine-minted messageId hex. */
  async postMessage(
    communityId: string,
    channelId: string,
    body: string,
    replyTo?: string,
    mentions?: string[],
    attachments?: ChannelAttachmentDto[],
  ): Promise<string> {
    if (!this.adapter) throw new Error('ChannelMessageService.postMessage: adapter not connected');
    const bodyBytes = Array.from(new TextEncoder().encode(body));
    try {
      const messageId = await this.adapter.invoke('post_channel_message', {
        communityId,
        channelId,
        body: bodyBytes,
        replyTo,
        // Send an empty mention list as undefined so the backend never emits
        // a `mn: []` (which would change the signed bytes vs. a mention-less
        // post). The backend also normalizes this defensively.
        mentions: mentions && mentions.length > 0 ? mentions : undefined,
        // Same empty-as-undefined rule as mentions: an empty array would
        // change the signed bytes vs. an attachment-less post.
        attachments: attachments && attachments.length > 0 ? attachments : undefined,
      }) as string;
      return messageId;
    } catch (e: unknown) {
      // Tauri IPC rejections are raw strings in production (Error objects
      // only in tests). Normalize so callers reading `.message` keep the
      // validation detail (e.g. "too many mentions: N (max 64)" or a bad-hex
      // rejection from the new mentions path). Mirrors MessageService.send.
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(msg);
    }
  }

  /** Ingest a local file into CAS as a channel artifact. Returns the
   *  attachment descriptor (cid/mime/name/size/encrypted) to attach to a
   *  subsequent postMessage. `encrypt` defaults to true. */
  async ingestArtifact(
    communityId: string,
    sourcePath: string,
    opts?: { name?: string; mime?: string; encrypt?: boolean },
  ): Promise<ChannelAttachmentDto> {
    if (!this.adapter) throw new Error('ChannelMessageService.ingestArtifact: adapter not connected');
    try {
      return await this.adapter.invoke('ingest_channel_artifact', {
        communityId,
        sourcePath,
        name: opts?.name,
        mime: opts?.mime,
        encrypt: opts?.encrypt ?? true,
      }) as ChannelAttachmentDto;
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(msg);
    }
  }

  /** Download a channel artifact from CAS to `destPath`. Returns the number
   *  of bytes written. `channelId` is required: the backend authorizes the
   *  CID against that channel's log and derives the authoritative size from
   *  the signed attachment (so there is no expectedSize param). */
  async downloadArtifact(
    communityId: string,
    channelId: string,
    attachment: ChannelAttachmentDto,
    destPath: string,
    maxBytes?: number,
  ): Promise<number> {
    if (!this.adapter) throw new Error('ChannelMessageService.downloadArtifact: adapter not connected');
    try {
      return await this.adapter.invoke('download_channel_artifact', {
        communityId,
        channelId,
        cid: attachment.cid,
        destPath,
        maxBytes,
      }) as number;
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(msg);
    }
  }

  /** Fetch a channel artifact into memory for inline preview (image/text).
   *  Returns the decrypted plaintext bytes. The backend authorizes the CID
   *  against the channel's signed log and rejects anything over the preview cap
   *  (default 4 MiB), so callers should only preview `isPreviewable` attachments.
   *  `maxBytes` further lowers the cap (clamped to the backend ceiling). */
  async previewArtifact(
    communityId: string,
    channelId: string,
    attachment: ChannelAttachmentDto,
    maxBytes?: number,
  ): Promise<Uint8Array> {
    if (!this.adapter) throw new Error('ChannelMessageService.previewArtifact: adapter not connected');
    try {
      const bytes = await this.adapter.invoke('preview_channel_artifact', {
        communityId,
        channelId,
        cid: attachment.cid,
        maxBytes,
      }) as number[];
      return new Uint8Array(bytes);
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(msg);
    }
  }

  /** Page through locally-known messages. Caches results + notifies
   *  subscribers (so callers don't double-render — list-then-subscribe
   *  is the standard pattern).
   *
   *  Returns the **newest** `limit` messages (ZEB-789 made that the
   *  backend default). `order` is deliberately not passed: the default is
   *  now what every caller here wants, and re-stating it at each call site
   *  invites someone to "fix" one of them to `'asc'`.
   *
   *  Response sequence does not matter to callers — `ingest` places each
   *  DTO by HLC-sorted insertion, so the rendered feed is chronological
   *  regardless of which end the backend walked from. That independence is
   *  why flipping the default was safe here. */
  async listMessages(
    communityId: string,
    channelId: string,
    since: HlcDto | undefined,
    limit: number,
  ): Promise<ChannelMessageDto[]> {
    if (!this.adapter) throw new Error('ChannelMessageService.listMessages: adapter not connected');
    const dtos = await this.adapter.invoke('list_channel_messages', {
      communityId,
      channelId,
      since,
      limit,
    }) as ChannelMessageDto[];
    for (const dto of dtos) {
      this.ingest(communityId, channelId, dto);
    }
    return dtos;
  }

  /** Fire-and-forget backfill request. Single-in-flight per
   *  (communityId, channelId) per spec §6.7 — additional calls during
   *  in-flight are no-ops. The gate releases when:
   *    1. The IPC promise rejects (engine error / not-connected), OR
   *    2. A terminal `channel-backfill-progress` event arrives (success).
   *
   *  IMPORTANT: This `await`s the IPC dispatch but the IPC itself returns
   *  immediately (fire-and-forget on the backend). The gate is held until
   *  a terminal progress event arrives via the channel-backfill-progress
   *  listener (which calls inFlightBackfill.delete() in connectAdapter).
   *  This is the correct spec §6.7 behavior — gate represents "backfill
   *  is producing packets", not "IPC dispatch is in-flight". */
  async requestBackfill(
    communityId: string,
    channelId: string,
    since?: HlcDto,
  ): Promise<void> {
    if (!this.adapter) throw new Error('ChannelMessageService.requestBackfill: adapter not connected');
    const key = chKey(communityId, channelId);
    if (this.inFlightBackfill.has(key)) return;
    this.inFlightBackfill.add(key);
    try {
      await this.adapter.invoke('request_channel_backfill', {
        communityId,
        channelId,
        since,
      });
      // Don't release the gate here — wait for a terminal progress tick,
      // because the IPC returns immediately (fire-and-forget) and packets
      // arrive afterward via channel-message-received.
    } catch (e) {
      this.inFlightBackfill.delete(key);
      throw e;
    }
  }

  /** Subscribe to live + backfilled messages on a single channel.
   *  Returns an unsubscribe function. Multiple subscribers per channel
   *  are supported (each callback fires once per ingested message). */
  subscribeToChannel(
    communityId: string,
    channelId: string,
    callback: (msg: ChannelMessageDto) => void,
  ): () => void {
    const key = chKey(communityId, channelId);
    let set = this.subscribers.get(key);
    if (!set) {
      set = new Set();
      this.subscribers.set(key, set);
    }
    set.add(callback);
    return () => {
      const s = this.subscribers.get(key);
      s?.delete(callback);
      if (s?.size === 0) this.subscribers.delete(key);
    };
  }

  /** Read-only snapshot of cached messages for a channel (oldest-first). */
  getMessages(communityId: string, channelId: string): ChannelMessageDto[] {
    return this.byChannel.get(chKey(communityId, channelId)) ?? [];
  }

  /** Test-helper / belt-and-braces: clear local in-flight gate.
   *  Production code shouldn't need this — the terminal-progress event
   *  releases it. Exposed for the rare error-recovery path where the
   *  engine drops without progress events. */
  clearBackfillInFlight(communityId: string, channelId: string): void {
    this.inFlightBackfill.delete(chKey(communityId, channelId));
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
    this.byChannel.clear();
    this.subscribers.clear();
    this.inFlightBackfill.clear();
    this.seenIds.clear();
    this.emojiNamesChangedListeners.clear();
    this.selfOwnerId = null;
    this.adapter = null;
  }

  private ingest(communityId: string, channelId: string, message: ChannelMessageDto): void {
    const key = chKey(communityId, channelId);
    let seen = this.seenIds.get(key);
    if (!seen) {
      seen = new Set();
      this.seenIds.set(key, seen);
    }
    if (seen.has(message.messageId)) return;
    seen.add(message.messageId);

    let arr = this.byChannel.get(key);
    if (!arr) {
      arr = [];
      this.byChannel.set(key, arr);
    }
    // Insert in HLC-sorted order. Comparison is wallMs primary, logical
    // secondary, deviceId tertiary — same convention as backend
    // list_channels' sort and ChannelLog's manifest.
    const idx = sortedInsertIndex(arr, message);
    arr.splice(idx, 0, message);

    try {
      this.onMessage?.(communityId, channelId, message);
    } catch (e) {
      console.error(`ChannelMessageService onMessage failed for ${key}:`, e);
    }
    this.notifyChannelSubscribers(key, message);
  }

  /** Fan out to this channel's subscribers only (no onMessage — used by
   *  both ingest and applyReaction; the latter must not trigger the
   *  onMessage roster-refetch path). */
  private notifyChannelSubscribers(key: string, message: ChannelMessageDto): void {
    const subs = this.subscribers.get(key);
    if (subs) {
      for (const cb of subs) {
        try {
          cb(message);
        } catch (e) {
          console.error(`ChannelMessageService subscriber failed for ${key}:`, e);
        }
      }
    }
  }

  /**
   * ZEB-536 Spec 2 — apply a live reaction event in place. Finds the
   * cached message by id (drops if not loaded — list will carry the
   * materialized reactions when it loads), then add/removes the reactor
   * from the emoji's `reactors` set, recomputes `count`/`mine`, and
   * notifies the channel's subscribers so the feed re-renders. Plain set
   * semantics — `at` is ignored (no frontend LWW in v1).
   */
  private applyReaction(p: ChannelReactionReceivedPayload): void {
    const key = chKey(p.communityId, p.channelId);
    const arr = this.byChannel.get(key);
    if (!arr) return;
    const msg = arr.find((m) => m.messageId === p.messageId);
    if (!msg) return;

    const reactions = msg.reactions ?? (msg.reactions = []);
    // A custom (image) reaction is keyed by its CID — its `emoji` is "" so it
    // would otherwise collide with every other custom reaction. Unicode
    // reactions key by the emoji string as before.
    const idx = p.emojiCid
      ? reactions.findIndex((r) => r.emojiCid === p.emojiCid)
      : reactions.findIndex((r) => !r.emojiCid && r.emoji === p.emoji);

    if (p.add) {
      let entry = idx >= 0 ? reactions[idx] : undefined;
      if (!entry) {
        entry = { emoji: p.emoji, count: 0, mine: false, reactors: [] };
        // Carry the custom-emoji descriptor so the feed can render the image.
        // `encrypted` (ZEB-541) gates the live "name this emoji" affordance —
        // naming is public-only, so an encrypted live chip must not offer it
        // (previously it stayed `undefined` until a channel reseed).
        if (p.emojiCid) {
          entry.emojiCid = p.emojiCid;
          entry.emojiSize = p.emojiSize;
          entry.encrypted = p.encrypted;
        }
        reactions.push(entry);
      }
      if (!entry.reactors.includes(p.reactor)) {
        entry.reactors.push(p.reactor);
      }
      entry.count = entry.reactors.length;
      // Preserve authoritative `mine` while the owner id is unknown (Qodo #318).
      if (this.selfOwnerId) entry.mine = entry.reactors.includes(this.selfOwnerId);
    } else {
      if (idx < 0) return; // unknown emoji — nothing to remove
      const entry = reactions[idx];
      entry.reactors = entry.reactors.filter((a) => a !== p.reactor);
      if (entry.reactors.length === 0) {
        reactions.splice(idx, 1);
      } else {
        entry.count = entry.reactors.length;
        // Preserve authoritative `mine` while the owner id is unknown (Qodo #318).
        if (this.selfOwnerId) entry.mine = entry.reactors.includes(this.selfOwnerId);
      }
    }

    this.notifyChannelSubscribers(key, msg);
  }

  /**
   * Recompute cached reaction `mine` flags against `self` and notify affected
   * channels, so a late-loading identity heals already-rendered reactions
   * without a channel reload (Qodo PR #318).
   */
  private recomputeMineFlags(self: string): void {
    for (const [key, arr] of this.byChannel) {
      for (const msg of arr) {
        if (!msg.reactions) continue;
        let changed = false;
        for (const r of msg.reactions) {
          const mine = r.reactors.includes(self);
          if (r.mine !== mine) {
            r.mine = mine;
            changed = true;
          }
        }
        if (changed) this.notifyChannelSubscribers(key, msg);
      }
    }
  }

  /**
   * Set or clear the local member's reaction on a message. Fire-and-forget;
   * the result returns to the feed via the channel-reaction-received event
   * (the backend echoes local React events back through the same path).
   *
   * For a unicode reaction, pass the emoji string and omit `customEmoji` — the
   * IPC payload is then byte-identical to the pre-custom-emoji behavior. For a
   * custom (CAS-backed image) reaction (ZEB-541), pass `emoji: ''` and a
   * `customEmoji` descriptor (the CID + advisory size of the already-ingested
   * PNG); the key is forwarded as `customEmoji` so the backend authorizes +
   * signs the CID.
   */
  async reactToMessage(
    communityId: string,
    channelId: string,
    messageId: string,
    emoji: string,
    add: boolean,
    customEmoji?: { cid: string; mime: string; size: number },
  ): Promise<void> {
    if (!this.adapter) throw new Error('ChannelMessageService.reactToMessage: adapter not connected');
    try {
      // Build the payload so `customEmoji` is ENTIRELY ABSENT for unicode
      // reactions (spreading `undefined` would still serialize a key on some
      // adapters; omitting it keeps the unicode path byte-identical).
      const args: Record<string, unknown> = { communityId, channelId, messageId, emoji, add };
      if (customEmoji) args.customEmoji = customEmoji;
      await this.adapter.invoke('set_message_reaction', args);
    } catch (e: unknown) {
      // Tauri IPC rejections are raw strings in production (Error objects only
      // in tests). Normalize so callers reading `.message` keep the rejection
      // detail. Mirrors postMessage / MessageService.send.
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(msg);
    }
  }

  /**
   * Ingest already-normalized PNG bytes (from {@link normalizeEmoji}) into CAS
   * for use as a custom reaction emoji. Public by default — a custom emoji is
   * `hash(plaintext)`-addressed so the same image is one CID network-wide
   * (deduplicated and freely served, never expiring). Pass `encrypted = true` to
   * keep this emoji private to the community (access-controlled, but the
   * permanence/epoch caveats of the encrypted path apply). Returns the minted CID
   * (hex) + plaintext size to pass to {@link reactToMessage} as the `customEmoji`
   * descriptor. The backend enforces a 256 KiB cap; an over-cap input rejects.
   */
  async ingestEmojiBytes(
    communityId: string,
    bytes: Uint8Array,
    encrypted: boolean = false,
  ): Promise<{ cid: string; size: number }> {
    if (!this.adapter) throw new Error('ChannelMessageService.ingestEmojiBytes: adapter not connected');
    try {
      const dto = await this.adapter.invoke('ingest_channel_artifact_bytes', {
        communityId,
        bytes: Array.from(bytes),
        name: '',
        mime: 'image/png',
        encrypt: encrypted,
      }) as ChannelAttachmentDto;
      return { cid: dto.cid, size: dto.size };
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(msg);
    }
  }

  /**
   * Fetch a custom reaction emoji's plaintext PNG bytes from CAS for inline
   * render (ZEB-541). The backend authorizes the CID against the channel's
   * signed React events and enforces a 256 KiB cap (no client-supplied max).
   * Returns the decrypted bytes. Mirrors {@link previewArtifact}.
   */
  async previewReactionEmoji(
    communityId: string,
    channelId: string,
    cid: string,
  ): Promise<Uint8Array> {
    if (!this.adapter) throw new Error('ChannelMessageService.previewReactionEmoji: adapter not connected');
    try {
      const bytes = await this.adapter.invoke('preview_reaction_emoji', {
        communityId,
        channelId,
        cid,
      }) as number[];
      return new Uint8Array(bytes);
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(msg);
    }
  }

  /**
   * Set (or clear, with `null`) the LOCAL-ONLY personal name for a PUBLIC custom
   * emoji. `mime`/`size` are the descriptor already known to the caller. The
   * backend emits `emoji-names-changed`, so subscribed UIs re-fetch.
   */
  async setEmojiName(cid: string, name: string | null, mime: string, size: number): Promise<void> {
    if (!this.adapter) throw new Error('ChannelMessageService.setEmojiName: adapter not connected');
    try {
      await this.adapter.invoke('set_emoji_name', { cid, name, mime, size });
    } catch (e: unknown) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }

  /** List the user's named emoji for the picker. */
  async listEmojiNames(): Promise<EmojiNameDto[]> {
    if (!this.adapter) throw new Error('ChannelMessageService.listEmojiNames: adapter not connected');
    try {
      return (await this.adapter.invoke('list_emoji_names', {})) as EmojiNameDto[];
    } catch (e: unknown) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }

  /** Fetch a NAMED (public) emoji's bytes by CID with no channel scope. */
  async previewNamedEmoji(cid: string): Promise<Uint8Array> {
    if (!this.adapter) throw new Error('ChannelMessageService.previewNamedEmoji: adapter not connected');
    try {
      const bytes = (await this.adapter.invoke('preview_named_emoji', { cid })) as number[];
      return new Uint8Array(bytes);
    } catch (e: unknown) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }

  /**
   * Register a callback fired when the backend emits `emoji-names-changed`
   * (the local emoji-name map was set/cleared, possibly on another device).
   * Receivers should re-fetch `listEmojiNames()`. Returns an unsubscribe
   * function; call it (e.g. in a component's `onDestroy`) to remove ONLY this
   * listener without disturbing others. Multiple subscribers are supported.
   */
  onEmojiNamesChanged(cb: () => void): () => void {
    this.emojiNamesChangedListeners.add(cb);
    return () => {
      this.emojiNamesChangedListeners.delete(cb);
    };
  }
}

function sortedInsertIndex(arr: ChannelMessageDto[], msg: ChannelMessageDto): number {
  // Linear scan from the end is fine for typical visible-window sizes
  // (~100s). When ChannelLog ships a windowed-prefetch optimization in
  // v3 we can swap to binary search; YAGNI for v2.
  for (let i = arr.length - 1; i >= 0; i--) {
    if (compareHlc(arr[i].at, msg.at) <= 0) return i + 1;
  }
  return 0;
}
