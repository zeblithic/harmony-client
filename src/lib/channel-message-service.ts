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
  ): Promise<string> {
    if (!this.adapter) throw new Error('ChannelMessageService.postMessage: adapter not connected');
    const bodyBytes = Array.from(new TextEncoder().encode(body));
    const messageId = await this.adapter.invoke('post_channel_message', {
      communityId,
      channelId,
      body: bodyBytes,
      replyTo,
    }) as string;
    return messageId;
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

    this.onMessage?.(communityId, channelId, message);
    const subs = this.subscribers.get(key);
    if (subs) for (const cb of subs) cb(message);
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
