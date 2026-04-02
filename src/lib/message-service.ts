import type { TauriAdapter } from './zenoh-service';
import type { Message, MessagePriority } from './types';
import { messages as mockMessages, profileStore } from './mock-data';

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

  private adapter: TauriAdapter | null = null;
  private unlisten?: () => void;

  constructor() {
    // Seed with mock data — real messages append on top.
    this.messages = [...mockMessages];
  }

  /** Connect a Tauri adapter and start listening for network messages. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    this.adapter = adapter;
    this.unlisten = await adapter.listen(
      'message-received',
      (event) => {
        const wire = event.payload as ChannelMessageEvent;
        const msg = this.wireToMessage(wire);
        this.messages = [...this.messages, msg];
        this.onChange?.();
      },
    ) as unknown as () => void;
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
          message: { channel, hub, text, priority, replyTo },
        });
        return; // Backend will echo via subscription → message-received event
      } catch {
        // Fall through to local-only append if not connected
      }
    }

    // Offline fallback: append locally so the UI stays responsive.
    const msg: Message = {
      id: `msg-${Date.now()}`,
      sender: { address: 'self', displayName: 'You' },
      text,
      timestamp: Date.now(),
      media: [],
      priority,
      replyTo,
    };
    this.messages = [...this.messages, msg];
    this.onChange?.();
  }

  /** Convert wire format to frontend Message type. */
  private wireToMessage(wire: ChannelMessageEvent): Message {
    // Resolve sender display name from profile store, fall back to wire value.
    const knownProfile = profileStore.get(wire.senderAddress);
    return {
      id: wire.id,
      sender: {
        address: wire.senderAddress,
        displayName: knownProfile?.displayName ?? wire.senderName ?? wire.senderAddress.slice(0, 8),
      },
      text: wire.text,
      timestamp: wire.timestamp,
      media: [],
      priority: (wire.priority as MessagePriority) || 'standard',
      replyTo: wire.replyTo,
    };
  }

  destroy(): void {
    this.unlisten?.();
    this.unlisten = undefined;
  }
}
