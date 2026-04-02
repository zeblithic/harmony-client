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
    ) as unknown as () => void;
    this.unlisteners.push(unlisten);
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
