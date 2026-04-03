import type { TauriAdapter, ProfilePayload } from './zenoh-service';
import type { NavNode, Profile } from './types';
import type { AvatarResolver } from './avatar-resolver';
import { navNodes as mockNavNodes, profileStore as mockProfileStore } from './mock-data';

/**
 * Manages navigation tree state and peer profile lookups.
 *
 * Seeds with mock data on construction. When connected via Tauri adapter,
 * listens for `profile-update` events to keep the profile map live, and
 * `nav-updated` events (future) to receive nav tree changes from the backend.
 */
export class NavService {
  nodes: NavNode[] = [];
  profiles: Map<string, Profile> = new Map();
  /** Called whenever nodes or profiles change so the UI can re-render. */
  onChange?: () => void;
  /** Hex-encoded own address — profile updates matching this are filtered. */
  ownAddress: string | null = null;

  private adapter: TauriAdapter | null = null;
  private avatarResolver: AvatarResolver | null = null;
  private unlisteners: Array<() => void> = [];

  constructor() {
    this.nodes = [...mockNavNodes];
    this.profiles = new Map(mockProfileStore);
  }

  /** Attach an avatar resolver for CID → blob URL resolution. */
  setAvatarResolver(resolver: AvatarResolver): void {
    this.avatarResolver = resolver;
  }

  /** Resolve an avatar CID to a blob URL (if available), falling back to avatarUrl. */
  private resolveAvatarUrl(wire: ProfilePayload): string | undefined {
    if (wire.avatarUrl) return wire.avatarUrl;
    const cid = wire.avatarMiniCid ?? wire.avatarCid;
    if (cid && this.avatarResolver) {
      return this.avatarResolver.resolve(cid);
    }
    return undefined;
  }

  /** Connect a Tauri adapter and start listening for profile + nav updates. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    const unlistenProfile = await adapter.listen(
      'profile-update',
      (event) => {
        const wire = event.payload as ProfilePayload;
        // Filter own profile echoes
        if (this.ownAddress && wire.address === this.ownAddress) return;
        const avatarUrl = this.resolveAvatarUrl(wire);
        this.profiles.set(wire.address, {
          address: wire.address,
          displayName: wire.displayName,
          statusText: wire.statusText,
          avatarUrl,
          avatarCid: wire.avatarCid,
          avatarMiniCid: wire.avatarMiniCid,
        });
        // Update DM nodes to match latest peer profile
        let nodeChanged = false;
        const updated = this.nodes.map((n) => {
          if (n.peer?.address !== wire.address) return n;
          const peerChanged = n.name !== wire.displayName || n.peer.avatarUrl !== avatarUrl;
          if (!peerChanged) return n;
          nodeChanged = true;
          return {
            ...n,
            name: wire.displayName,
            peer: { ...n.peer, displayName: wire.displayName, avatarUrl },
          };
        });
        if (nodeChanged) this.nodes = updated;
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlistenProfile);
  }

  /** Look up a peer's status text by address. */
  profileLookup(address: string): string | undefined {
    return this.profiles.get(address)?.statusText;
  }

  /** Look up a full profile by address (for popovers). */
  getProfile(address: string): Profile | undefined {
    return this.profiles.get(address);
  }

  /** Re-check resolved avatar URLs after the AvatarResolver fetches new content. */
  refreshAvatars(): void {
    if (!this.avatarResolver) return;
    let changed = false;
    for (const [, profile] of this.profiles) {
      // Don't override a direct avatarUrl with a CID blob URL.
      if (profile.avatarUrl && !profile.avatarUrl.startsWith('blob:')) continue;
      const cid = profile.avatarMiniCid ?? profile.avatarCid;
      if (!cid) continue;
      const resolved = this.avatarResolver.resolve(cid);
      if (resolved && profile.avatarUrl !== resolved) {
        profile.avatarUrl = resolved;
        changed = true;
      }
    }
    if (!changed) return;
    // Sync DM nodes with updated avatarUrls
    let nodeChanged = false;
    const updated = this.nodes.map((n) => {
      if (!n.peer) return n;
      const profile = this.profiles.get(n.peer.address);
      if (!profile || n.peer.avatarUrl === profile.avatarUrl) return n;
      nodeChanged = true;
      return { ...n, peer: { ...n.peer, avatarUrl: profile.avatarUrl } };
    });
    if (nodeChanged) this.nodes = updated;
    this.onChange?.();
  }

  addUnlisten(fn: () => void): void {
    this.unlisteners.push(fn);
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
  }
}
