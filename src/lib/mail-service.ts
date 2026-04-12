import type { TauriAdapter } from './zenoh-service';
import type { MailEntry, MailMessageDetail, MailFolderKind, MailCounts, MailFolderCounts } from './types';
import { mockMailEntries, mockMailCounts } from './mock-mail-data';

/**
 * Service for managing email via harmony-mail CAS backend.
 * Follows the same pattern as MessageService and VineService.
 */
export class MailService {
  entries: MailEntry[] = [];
  activeFolder: MailFolderKind = 'inbox';
  counts: MailCounts = { inbox: { total: 0, unread: 0 }, sent: { total: 0, unread: 0 }, drafts: { total: 0, unread: 0 }, trash: { total: 0, unread: 0 } };
  onChange?: () => void;
  ownAddress: string | null = null;

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];
  private seenCids = new Set<string>();

  constructor() {
    this.entries = [...mockMailEntries];
    this.counts = { ...mockMailCounts };
    for (const e of this.entries) this.seenCids.add(e.messageCid);
  }

  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    // Listen for incoming mail events from the Rust backend.
    const unlisten = await adapter.listen('mail-received', (event) => {
      const entry = event.payload as MailEntry;
      if (this.seenCids.has(entry.messageCid)) return;
      this.seenCids.add(entry.messageCid);
      // Only prepend to the visible list if we're viewing the inbox
      if (this.activeFolder === 'inbox') {
        this.entries.unshift(entry);
      }
      this.counts.inbox.total += 1;
      this.counts.inbox.unread += 1;
      this.onChange?.();
    });
    this.unlisteners.push(unlisten);

    // Load initial state from backend.
    await this.refreshCounts();
    await this.loadFolder(this.activeFolder);
  }

  async loadFolder(folder: MailFolderKind, page = 0): Promise<void> {
    this.activeFolder = folder;
    if (!this.adapter) return;
    try {
      const entries = await this.adapter.invoke('list_mail', {
        folder,
        page,
        perPage: 50,
      }) as MailEntry[];
      this.entries = entries;
      this.seenCids.clear();
      for (const e of entries) this.seenCids.add(e.messageCid);
      this.onChange?.();
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      if (!msg.includes('not connected') && !msg.includes('mail not initialized')) throw err;
    }
  }

  async getMessage(cid: string): Promise<MailMessageDetail | null> {
    if (!this.adapter) return null;
    try {
      return await this.adapter.invoke('get_mail', { messageCid: cid }) as MailMessageDetail;
    } catch {
      return null;
    }
  }

  async send(to: string[], subject: string, body: string, replyTo?: string): Promise<void> {
    if (!this.adapter) return;
    await this.adapter.invoke('send_mail', {
      payload: { to, subject, body, replyTo: replyTo ?? null },
    });
    await this.refreshCounts();
  }

  async markRead(cid: string): Promise<void> {
    if (this.adapter) {
      try {
        await this.adapter.invoke('update_mail', { messageCid: cid, action: 'mark_read' });
      } catch {
        // Fall through to local update
      }
    }
    const entry = this.entries.find(e => e.messageCid === cid);
    if (entry && !entry.read) {
      entry.read = true;
      const folderCounts = this.counts[this.activeFolder];
      if (folderCounts) folderCounts.unread = Math.max(0, folderCounts.unread - 1);
      this.onChange?.();
    }
  }

  async moveToTrash(cid: string): Promise<void> {
    if (this.adapter) {
      try {
        await this.adapter.invoke('update_mail', { messageCid: cid, action: 'move_trash' });
      } catch {
        // Fall through to local removal
      }
    }
    const idx = this.entries.findIndex(e => e.messageCid === cid);
    if (idx !== -1) {
      const entry = this.entries.splice(idx, 1)[0];
      const folderCounts = this.counts[this.activeFolder];
      if (folderCounts) {
        folderCounts.total = Math.max(0, folderCounts.total - 1);
        if (!entry.read) folderCounts.unread = Math.max(0, folderCounts.unread - 1);
      }
      this.counts.trash.total += 1;
      if (!entry.read) this.counts.trash.unread += 1;
      this.onChange?.();
    }
  }

  async refreshCounts(): Promise<void> {
    if (!this.adapter) return;
    try {
      const counts = await this.adapter.invoke('get_mail_counts', {}) as Record<string, MailFolderCounts>;
      this.counts = {
        inbox: counts['inbox'] ?? { total: 0, unread: 0 },
        sent: counts['sent'] ?? { total: 0, unread: 0 },
        drafts: counts['drafts'] ?? { total: 0, unread: 0 },
        trash: counts['trash'] ?? { total: 0, unread: 0 },
      };
      this.onChange?.();
    } catch {
      // Keep mock/local counts
    }
  }

  addUnlisten(fn: () => void): void {
    this.unlisteners.push(fn);
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
  }
}
