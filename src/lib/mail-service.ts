import type { TauriAdapter } from './zenoh-service';
import type { MailEntry, MailMessageDetail, MailFolderKind, MailCounts, MailFolderCounts } from './types';

/**
 * Service for managing email via harmony-mail CAS backend.
 * Follows the same pattern as MessageService and VineService.
 *
 * Starts empty — real data loads via connectAdapter(). No mock data is
 * seeded in production; users see "No messages" until the backend connects.
 */
export class MailService {
  entries: MailEntry[] = [];
  activeFolder: MailFolderKind = 'inbox';
  counts: MailCounts = { inbox: { total: 0, unread: 0 }, sent: { total: 0, unread: 0 }, drafts: { total: 0, unread: 0 }, trash: { total: 0, unread: 0 } };
  syncState: 'idle' | 'syncing' | 'error' = 'idle';
  syncError: string | null = null;
  onChange?: () => void;

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];
  private seenCids = new Set<string>();
  private inboxSeenCids = new Set<string>();
  private loadRequestId = 0;

  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    // Listen for incoming mail events from the Rust backend.
    const unlisten = await adapter.listen('mail-received', (event) => {
      const entry = event.payload as MailEntry;
      // Dedupe against inbox-specific set (not seenCids which tracks the
      // current folder view and would block self-sends visible in "sent")
      if (this.inboxSeenCids.has(entry.messageCid)) return;
      this.inboxSeenCids.add(entry.messageCid);
      // Only prepend to the visible list if we're viewing the inbox
      if (this.activeFolder === 'inbox') {
        this.entries.unshift(entry);
        this.seenCids.add(entry.messageCid);
      }
      this.counts.inbox.total += 1;
      this.counts.inbox.unread += 1;
      this.onChange?.();
    });
    this.unlisteners.push(unlisten);

    // Listen for walker/sync state updates.
    const syncUnlisten = await adapter.listen('mail-sync-status', (event) => {
      const payload = event.payload as { state: string; error?: string };
      const newState = payload.state as 'idle' | 'syncing' | 'error';
      const newError = payload.error ?? null;
      if (this.syncState !== newState || this.syncError !== newError) {
        this.syncState = newState;
        this.syncError = newError;
        this.onChange?.();
      }
    });
    this.unlisteners.push(syncUnlisten);

    // Load initial state from backend.
    await this.refreshCounts();
    await this.loadFolder(this.activeFolder);
  }

  async loadFolder(folder: MailFolderKind, page = 0): Promise<void> {
    const requestId = ++this.loadRequestId;
    this.activeFolder = folder;
    // Clear stale entries immediately so the UI doesn't show the previous
    // folder's data while the new folder loads (or if the load fails).
    this.entries = [];
    this.seenCids.clear();
    this.onChange?.();
    if (!this.adapter) return;
    try {
      const entries = await this.adapter.invoke('list_mail', {
        folder,
        page,
        perPage: 50,
      }) as MailEntry[];
      if (requestId !== this.loadRequestId) return; // stale response
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
      const detail = await this.adapter.invoke('get_mail', { messageCid: cid }) as MailMessageDetail;
      if (detail.bodyState === 'pending') {
        // Body not yet cached — trigger lazy CAS fetch via MailSync.
        // Returns the now-Local MailDetail on success.
        return await this.adapter.invoke('fetch_mail_body', { messageCid: cid }) as MailMessageDetail;
      }
      return detail;
    } catch {
      return null;
    }
  }

  /**
   * Manually trigger a mailbox re-sync. Calls into the Rust MailSync
   * refresh_now path, which re-queries the gateway for the current
   * root CID and walks if it has changed.
   */
  async refresh(): Promise<void> {
    if (!this.adapter) return;
    try {
      await this.adapter.invoke('refresh_mail', {});
    } catch (err) {
      // Surface via syncState/syncError; the mail-sync-status listener
      // will handle most error reporting. Log here for diagnostics.
      const msg = err instanceof Error ? err.message : String(err);
      console.warn('refresh_mail failed:', msg);
    }
  }

  async send(to: string[], subject: string, body: string, replyTo?: string): Promise<void> {
    if (!this.adapter) return;
    await this.adapter.invoke('send_mail', {
      payload: { to, subject, body, replyTo: replyTo ?? null },
    });
    await this.refreshCounts();
    if (this.activeFolder === 'sent') {
      await this.loadFolder('sent');
    }
  }

  async markRead(cid: string, folder?: MailFolderKind): Promise<void> {
    const targetFolder = folder ?? this.activeFolder;
    if (this.adapter) {
      await this.adapter.invoke('update_mail', { messageCid: cid, action: 'mark_read', folder: targetFolder });
    }
    const entry = this.entries.find(e => e.messageCid === cid);
    if (entry && !entry.read) {
      entry.read = true;
      const folderCounts = this.counts[targetFolder];
      if (folderCounts) folderCounts.unread = Math.max(0, folderCounts.unread - 1);
      this.onChange?.();
    }
  }

  async moveToTrash(cid: string): Promise<void> {
    if (this.adapter) {
      await this.adapter.invoke('update_mail', { messageCid: cid, action: 'move_trash', folder: this.activeFolder });
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
