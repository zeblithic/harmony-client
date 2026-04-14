import type { TauriAdapter } from './zenoh-service';
import type { InboxEntry, MailMessage } from './types';

/**
 * Manages mail inbox state and message viewing.
 *
 * Listens for 'mail-root-updated' IPC events to refresh the inbox.
 * Follows the same service pattern as MessageService/VineService.
 */
export class MailService {
  entries: InboxEntry[] = [];
  selectedCid: string | null = null;
  selectedMessage: MailMessage | null = null;
  loading = false;
  /** Called whenever service state changes so the UI can re-render. */
  onChange: (() => void) | null = null;

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];
  private latestRootCid: string | null = null;

  /** Connect a Tauri adapter and start listening for mail events. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    const unlisten = await adapter.listen(
      'mail-root-updated',
      (event) => {
        const payload = event.payload as { rootCid: string };
        if (payload.rootCid) {
          this.latestRootCid = payload.rootCid;
          this.refreshInbox();
        }
      },
    );
    this.unlisteners.push(unlisten);
  }

  /** Refresh the inbox entries from the backend. */
  async refreshInbox(): Promise<void> {
    if (!this.adapter) return;
    this.loading = true;
    this.onChange?.();
    try {
      const entries = await this.adapter.invoke('get_inbox', {
        rootCid: this.latestRootCid,
      }) as InboxEntry[];
      this.entries = entries;
    } catch (err) {
      console.error('Failed to refresh inbox:', err);
    } finally {
      this.loading = false;
      this.onChange?.();
    }
  }

  /** Open a message by CID. */
  async openMessage(cid: string): Promise<void> {
    if (!this.adapter) return;
    this.selectedCid = cid;
    this.loading = true;
    this.onChange?.();
    try {
      const message = await this.adapter.invoke('get_mail_message', {
        messageCid: cid,
      }) as MailMessage;
      this.selectedMessage = message;
    } catch (err) {
      console.error('Failed to open message:', err);
      this.selectedMessage = null;
    } finally {
      this.loading = false;
      this.onChange?.();
    }
  }

  /** Close the selected message. */
  closeMessage(): void {
    this.selectedCid = null;
    this.selectedMessage = null;
    this.onChange?.();
  }

  /** Register an external unlisten handle for cleanup. */
  addUnlisten(fn: () => void): void {
    this.unlisteners.push(fn);
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
  }
}
