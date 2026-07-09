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

/** Runtime shape guard for persisted cursor entries — downstream HLC
 *  comparison assumes these exact field types. */
function isHlc(v: unknown): v is Hlc {
  return (
    typeof v === 'object' &&
    v !== null &&
    typeof (v as Hlc).wallMs === 'number' &&
    typeof (v as Hlc).logical === 'number' &&
    typeof (v as Hlc).deviceId === 'string'
  );
}

export interface UnreadCursorStore {
  connectOwner(ownerId: string): void;
  get(communityId: string, channelId: string): Hlc | null;
  set(communityId: string, channelId: string, hlc: Hlc): void;
}

export class LocalStorageUnreadCursorStore implements UnreadCursorStore {
  private ownerId: string | null = null;
  private map = new Map<string, Hlc>();

  connectOwner(ownerId: string): void {
    // ZEB-666: the instance is shared by ChannelUnreadService and
    // DmUnreadService, both of which connect the same owner — a same-owner
    // reconnect must not clear/reparse the live map out from under the
    // other service (every set() persists synchronously, so there is
    // nothing newer in localStorage to pick up).
    if (this.ownerId === ownerId) return;
    this.ownerId = ownerId;
    this.map = new Map();
    try {
      const raw = localStorage.getItem(this.key());
      if (raw) {
        const parsed = JSON.parse(raw) as Record<string, unknown>;
        for (const [k, v] of Object.entries(parsed)) {
          // Valid JSON isn't a valid cursor: drop mis-shaped entries (schema
          // drift / partial corruption) instead of feeding them to compareHlc.
          // A dropped entry degrades that one channel to start-clean.
          if (isHlc(v)) this.map.set(k, v);
        }
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
