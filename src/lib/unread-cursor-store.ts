/**
 * ZEB-665: owner-scoped persistence for per-channel read cursors ("newest-seen
 * message HLC"). localStorage in v1; the interface exists so the cross-device
 * OwnerState-CRDT swap (spec §8.1) is a storage-layer change only.
 *
 * Owner scoping follows the theme/profile-service pattern (the ZEB-586/589
 * fix): WebView localStorage is bundle-scoped, not identity-scoped, so a fixed
 * key would leak read state across owners on one machine. Before connectOwner,
 * reads return null and writes no-op.
 */
import type { Hlc } from './types';

const KEY_PREFIX = 'harmony-unread';

export interface UnreadCursorStore {
  connectOwner(ownerId: string): void;
  get(communityId: string, channelId: string): Hlc | null;
  set(communityId: string, channelId: string, hlc: Hlc): void;
}

export class LocalStorageUnreadCursorStore implements UnreadCursorStore {
  private ownerId: string | null = null;
  private map = new Map<string, Hlc>();

  connectOwner(ownerId: string): void {
    this.ownerId = ownerId;
    this.map = new Map();
    try {
      const raw = localStorage.getItem(this.key());
      if (raw) {
        const parsed = JSON.parse(raw) as Record<string, Hlc>;
        for (const [k, v] of Object.entries(parsed)) this.map.set(k, v);
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('[unread-cursor-store] corrupt blob, starting clean:', msg);
      this.map = new Map();
    }
  }

  get(communityId: string, channelId: string): Hlc | null {
    if (this.ownerId === null) return null;
    return this.map.get(`${communityId}:${channelId}`) ?? null;
  }

  set(communityId: string, channelId: string, hlc: Hlc): void {
    if (this.ownerId === null) return; // pre-identity: never write a shared key
    this.map.set(`${communityId}:${channelId}`, hlc);
    try {
      localStorage.setItem(this.key(), JSON.stringify(Object.fromEntries(this.map)));
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('[unread-cursor-store] persist failed:', msg);
    }
  }

  private key(): string {
    return `${KEY_PREFIX}:owner-${this.ownerId}`;
  }
}
