import type { TauriAdapter, ProfilePayload } from './zenoh-service';
import type { NavNode, NavNodeType, Profile } from './types';
import type { AvatarResolver } from './avatar-resolver';
import { navNodes as mockNavNodes, profileStore as mockProfileStore } from './mock-data';

/**
 * Wire shape of the `nav-updated` IPC event.
 *
 * Emitted by the backend when a Space CRDT entry is added, modified,
 * or removed. Currently handled kinds: `dm`, `group-dm` (Phase 4 /
 * ZEB-228), `community` (Phase 5 / ZEB-263 + emit-side ZEB-265).
 * `channel` and `folder` kinds are reserved for later phases and
 * silently ignored here.
 */
export interface NavUpdatedPayload {
  action: 'added' | 'removed' | 'modified';
  spaceId: string;
  kind: 'dm' | 'group-dm' | 'channel' | 'community' | 'folder';
  name: string;
  members?: string[];
  parentId?: string | null;
}

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
    // Seed with mock data for browser/dev mode — `connectAdapter()` clears
    // these before subscribing to real events (ZEB-209).
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

    // ZEB-209: clear mock-seeded state before subscribing to real events.
    // The constructor seeds mockNavNodes + mockProfileStore for browser/
    // dev mode (no adapter connects). In production the adapter always
    // wires in, so the mocks must go to avoid mock channels/DMs that are
    // uninhabitable (no real Zenoh state behind them).
    this.nodes = [];
    this.profiles = new Map();
    this.onChange?.();

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

    const unlistenNav = await adapter.listen('nav-updated', (event) => {
      this.addOrUpdateNavSpace(event.payload as NavUpdatedPayload);
    });
    this.unlisteners.push(unlistenNav);
  }

  /**
   * Phase 4 (ZEB-228) — apply a `nav-updated`-shaped payload to the
   * NavNode tree. Extracted from the `nav-updated` listener so the
   * frontend `add_space` IPC path (App.svelte:handleDmCreate) can
   * synthesize the same NavNode without depending on a backend emit
   * (no `nav-updated` emit exists on the Rust side yet — Fix B from
   * PR #81 review).
   *
   * Phase 5 (ZEB-263) extends this to handle `community` kind.
   */
  addOrUpdateNavSpace(payload: NavUpdatedPayload): void {
    const { action, spaceId, kind, name, members, parentId } = payload;

    if (kind === 'community') {
      if (action === 'removed') {
        const before = this.nodes.length;
        this.nodes = this.nodes.filter((n) => n.id !== spaceId);
        if (this.nodes.length !== before) this.onChange?.();
        return;
      }

      const newNode: NavNode = {
        id: spaceId,
        type: 'community',
        name,
        parentId: parentId ?? null,
        expanded: true, // default expanded; user can collapse
        unreadCount: 0,
        unreadLevel: 'none',
        peer: undefined,
      };

      if (action === 'added') {
        const existing = this.nodes.find((n) => n.id === spaceId);
        if (existing) {
          // Preserve user-applied state on duplicate add (cold-replay):
          // parentId (folder placement), expanded, and unread counters.
          this.nodes = this.nodes.map((n) =>
            n.id === spaceId
              ? {
                  ...newNode,
                  parentId: existing.parentId,
                  expanded: existing.expanded,
                  unreadCount: existing.unreadCount,
                  unreadLevel: existing.unreadLevel,
                }
              : n
          );
        } else {
          this.nodes = [...this.nodes, newNode];
        }
      } else if (action === 'modified') {
        let found = false;
        this.nodes = this.nodes.map((n) => {
          if (n.id !== spaceId) return n;
          found = true;
          return { ...n, name }; // preserve existing parentId/expanded/unread state
        });
        if (!found) this.nodes = [...this.nodes, newNode];
      }

      this.onChange?.();
      return;
    }

    // Phase 5 (ZEB-263) handles dm/group-dm/community kinds.
    // Channel kind is reserved for the channel-introduction phase
    // and silently ignored here.
    if (kind !== 'dm' && kind !== 'group-dm') return;

    if (action === 'removed') {
      const before = this.nodes.length;
      this.nodes = this.nodes.filter((n) => n.id !== spaceId);
      if (this.nodes.length !== before) this.onChange?.();
      return;
    }

    const navType: NavNodeType = kind === 'dm' ? 'dm' : 'group-chat';
    // Fix F from PR #81 review: backend's `add_space` puts BOTH self
    // and peer in `members` (sorted, deduped). For 1:1 DMs the peer is
    // whichever member isn't us. Fall back to members[0] in the
    // pre-bootstrap case where ownAddress isn't set yet, and to
    // members.length===1 (legacy / test-only shape) for safety. Group
    // DMs (kind='group-dm') never get a single-peer attachment.
    let peerAddress: string | undefined;
    if (kind === 'dm' && members && members.length > 0) {
      if (this.ownAddress) {
        peerAddress = members.find((a) => a !== this.ownAddress) ?? members[0];
      } else {
        peerAddress = members[0];
      }
    }
    const peerProfile = peerAddress ? this.profiles.get(peerAddress) : undefined;
    const peer = peerAddress
      ? {
          address: peerAddress,
          displayName: peerProfile?.displayName ?? name,
          avatarUrl: peerProfile?.avatarUrl,
        }
      : undefined;
    const newNode: NavNode = {
      id: spaceId,
      type: navType,
      name,
      parentId: parentId ?? null,
      expanded: false,
      unreadCount: 0,
      unreadLevel: 'none',
      peer,
    };

    if (action === 'added') {
      // Fix G from PR #81 review: a duplicate `added` (reconnect /
      // cold-start replay) must not wipe user-applied UI state. Preserve
      // parentId (folder placement), expanded, and unread counters from
      // the existing node. Round 3: also preserve the cached peer when
      // the replayed payload omits members (mirrors what the modified
      // path already does — without this, a name-only re-emit would
      // drop displayName/avatarUrl back to undefined).
      const existing = this.nodes.find((n) => n.id === spaceId);
      if (existing) {
        const peerWasDerivedFromPayload = members !== undefined;
        const merged: NavNode = {
          ...newNode,
          parentId: existing.parentId,
          expanded: existing.expanded,
          unreadCount: existing.unreadCount,
          unreadLevel: existing.unreadLevel,
          peer: peerWasDerivedFromPayload ? newNode.peer : existing.peer,
        };
        this.nodes = this.nodes.map((n) => (n.id === spaceId ? merged : n));
      } else {
        this.nodes = [...this.nodes, newNode];
      }
    } else if (action === 'modified') {
      // Patch in-place — preserve existing parentId/expanded/unread state
      // so user-applied folder placement and read state aren't clobbered
      // by a name change. Also preserve cached `peer` when the modified
      // payload omits `members` (name-only update); only overwrite peer
      // when the new payload actually carried member info we could
      // derive a fresh peer from.
      const peerWasDerivedFromPayload = members !== undefined;
      let found = false;
      this.nodes = this.nodes.map((n) => {
        if (n.id !== spaceId) return n;
        found = true;
        return { ...n, name, peer: peerWasDerivedFromPayload ? peer : n.peer };
      });
      if (!found) {
        // Modified for an unknown spaceId — treat as added to stay
        // self-healing on missed `added` events.
        this.nodes = [...this.nodes, newNode];
      }
    }
    this.onChange?.();
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
