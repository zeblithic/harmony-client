import type { TauriAdapter } from './zenoh-service';
import type { Message, MessagePriority } from './types';
import { messages as mockMessages } from './mock-data';

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
  /** Hex-encoded node address — set after Zenoh connects so we can
   *  identify self-sent messages in the echo. */
  ownAddress: string | null = null;
  /** Display name to include in outgoing messages. */
  ownDisplayName = 'You';

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];
  private seenIds = new Set<string>();

  constructor() {
    // Seed with mock data — real messages append on top.
    this.messages = [...mockMessages];
    for (const m of this.messages) this.seenIds.add(m.id);
  }

  /** Connect a Tauri adapter and start listening for network messages. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return; // already wired; prevent duplicate listeners
    this.adapter = adapter;
    const unlisten = await adapter.listen(
      'message-received',
      (event) => {
        const wire = event.payload as ChannelMessageEvent;
        if (this.seenIds.has(wire.id)) return;
        this.seenIds.add(wire.id);
        const msg = this.wireToMessage(wire);
        this.messages = [...this.messages, msg];
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlisten);

    // ── Phase 4 (ZEB-228) — DM lifecycle subscriptions ──────────────
    //
    // The DM transport uses a separate IPC channel from channel pub/sub.
    // `dm-received` carries hex-encoded body bytes (UTF-8 text payloads
    // for now). The lifecycle events (`dm-delivered`/`dm-expired`/
    // `dm-deleted`) correlate to a self-Message via `messageId` (hex
    // OutboxEntryId set on send). `channel` on a DM Message is the
    // SpaceId hex, matching `NavNode.id` from Task 8.

    const unlistenDmRx = await adapter.listen('dm-received', (event) => {
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

      const text = hexToUtf8(payload.body);
      const sender = (this.ownAddress && payload.from === this.ownAddress)
        ? { address: 'self', displayName: 'You' }
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
      };
      this.messages = [...this.messages, msg];
      this.onChange?.();
    });
    this.unlisteners.push(unlistenDmRx);

    const unlistenDmDelivered = await adapter.listen('dm-delivered', (event) => {
      const { messageId } = event.payload as { messageId: string; recipient: string };
      let changed = false;
      this.messages = this.messages.map((m) => {
        if (m.messageId !== messageId) return m;
        changed = true;
        return { ...m, deliveryState: 'delivered' as const };
      });
      if (changed) this.onChange?.();
    });
    this.unlisteners.push(unlistenDmDelivered);

    const unlistenDmExpired = await adapter.listen('dm-expired', (event) => {
      const { messageId } = event.payload as { messageId: string };
      let changed = false;
      this.messages = this.messages.map((m) => {
        if (m.messageId !== messageId) return m;
        changed = true;
        return { ...m, deliveryState: 'expired' as const };
      });
      if (changed) this.onChange?.();
    });
    this.unlisteners.push(unlistenDmExpired);

    const unlistenDmDeleted = await adapter.listen('dm-deleted', (event) => {
      const { spaceId, messageCid } = event.payload as {
        messageId?: string;
        spaceId: string;
        messageCid: string;
      };
      const before = this.messages.length;
      this.messages = this.messages.filter(
        (m) => !(m.channel === spaceId && m.id === messageCid),
      );
      if (this.messages.length !== before) {
        // Drop from seenIds so a re-arrival of the same CID (e.g. peer
        // resends after we manually deleted a stuck entry) isn't deduped.
        this.seenIds.delete(messageCid);
        this.onChange?.();
      }
    });
    this.unlisteners.push(unlistenDmDeleted);
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
      sender: { address: 'self', displayName: 'You' },
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

  /** Convert wire format to frontend Message type. */
  private wireToMessage(wire: ChannelMessageEvent): Message {
    // Self-sent messages echo back via Zenoh — map to 'self'/'You'
    // so the rest of the UI (knownPeers filter, display name) works.
    const sender = (this.ownAddress && wire.senderAddress === this.ownAddress)
      ? { address: 'self', displayName: 'You' }
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
