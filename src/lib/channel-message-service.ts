import type { TauriAdapter } from './zenoh-service';

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
   * ZEB-536 — per-emoji reaction summaries. Optional: populated by the
   * Rust IPC boundary; consumed by the reactions UI (Spec 2).
   */
  reactions?: { emoji: string; count: number; mine: boolean; reactors: string[] }[];
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

  /** Page through locally-known messages. Caches results + notifies
   *  subscribers (so callers don't double-render — list-then-subscribe
   *  is the standard pattern). */
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

function compareHlc(a: HlcDto, b: HlcDto): number {
  if (a.wallMs !== b.wallMs) return a.wallMs - b.wallMs;
  if (a.logical !== b.logical) return a.logical - b.logical;
  return a.deviceId < b.deviceId ? -1 : a.deviceId > b.deviceId ? 1 : 0;
}
