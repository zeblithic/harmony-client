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
  /** ZEB-285: hex SpaceId of the original community, present only for forked communities. */
  forkedFrom?: string;
  /**
   * ZEB-254: present only on community `modified` payloads emitted by the
   * joiner-side pending-clear hook (JoinCountersign landed). `false` means
   * the pending state just cleared. Absent (`undefined`) on all other payloads.
   */
  pending?: boolean;
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
  private connecting = false;
  private avatarResolver: AvatarResolver | null = null;
  private unlisteners: Array<() => void> = [];
  /** IDs of constructor-seeded mock nav nodes — used to selectively clear
   *  mocks on `connectAdapter()` while preserving any locally-created
   *  entries from pre-connect-window calls (e.g. handleDmCreate calling
   *  addOrUpdateNavSpace before the adapter wires in) (ZEB-209 bot R1). */
  private mockNodeIds = new Set<string>();
  /** Keys of constructor-seeded mock profiles — used to selectively clear
   *  mocks on `connectAdapter()` while preserving any locally-added
   *  profiles from the pre-connect window (ZEB-209 bot R1). */
  private mockProfileKeys = new Set<string>();

  /**
   * @param opts.seedMockData Whether to seed the nav tree + profile map with
   *   mock data so the sidebar is never empty while iterating. Defaults to
   *   `import.meta.env.DEV` — true in `vite dev` / browser dev and under
   *   vitest, **false in a production `vite build`**. ZEB-560: the sidebar
   *   ships in the alpha surface, so a shipped build must NOT seed fake
   *   communities/friends — a real tester would otherwise see them. Gating at
   *   construction (rather than relying solely on the `connectAdapter()` clear)
   *   is the robust fix: that clear sits at the end of a long serial boot-time
   *   connect chain and never runs if an earlier service connect stalls,
   *   leaving the mock sidebar permanently visible on a real node. Mirrors
   *   VineService's ZEB-546 fix. Tests pass an explicit flag to exercise both
   *   modes deterministically.
   */
  constructor(opts?: { seedMockData?: boolean }) {
    const seedMockData = opts?.seedMockData ?? import.meta.env.DEV;
    if (!seedMockData) return;
    // Seed with mock data for browser/dev mode — `connectAdapter()` clears
    // these (selectively, preserving any locally-created entries from the
    // pre-connect window) before subscribing to real events (ZEB-209).
    this.nodes = [...mockNavNodes];
    this.profiles = new Map(mockProfileStore);
    this.mockNodeIds = new Set(mockNavNodes.map((n) => n.id));
    this.mockProfileKeys = new Set(mockProfileStore.keys());
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
    if (this.adapter || this.connecting) return; // already wired or in-progress; prevent duplicate listeners
    this.connecting = true;

    // ── R3: Selective clear FIRST so handlers see clean state ──
    // Preserves locally-created entries from pre-connect-window calls (e.g.
    // handleDmCreate calling addOrUpdateNavSpace before the adapter wires in).
    // By the time any listener handler can possibly fire, mock node ids and
    // profile keys are already removed, and mockNodeIds/mockProfileKeys are
    // empty — no mock-vs-real collision is possible. On listen failure the
    // service is in a clean-but-unconnected state; this.adapter is still null
    // so a retry is allowed, and the selective clear is idempotent.
    this.nodes = this.nodes.filter((n) => !this.mockNodeIds.has(n.id));
    for (const key of this.mockProfileKeys) this.profiles.delete(key);
    this.mockNodeIds = new Set();
    this.mockProfileKeys = new Set();
    this.onChange?.();

    // ZEB-209 bot-feedback round 1: register listeners FIRST so a partial-init
    // failure doesn't wedge the service. Adapter and unlisteners are committed
    // only after all listens succeed; on failure we tear down partial work and
    // rethrow so the caller (App.svelte tryConnect) can retry.
    // ZEB-209 bot-feedback round 2: `connecting` sentinel (above) prevents a
    // concurrent caller from passing the guard while awaits are in flight.
    // ZEB-209 bot-feedback round 3: selective clear moved above listener
    // registration so handlers see clean state between successive awaits.
    const localUnlisteners: Array<() => void> = [];
    try { // outer try — releases connecting sentinel in finally on any exit path
    try {
      localUnlisteners.push(await adapter.listen(
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
      ));

      localUnlisteners.push(await adapter.listen('nav-updated', (event) => {
        this.addOrUpdateNavSpace(event.payload as NavUpdatedPayload);
      }));
    } catch (err) {
      for (const fn of localUnlisteners) fn();
      // Note: mocks are already cleared and user-created entries preserved.
      // On listen failure the service is in a clean but unconnected state —
      // the App.svelte tryConnect wrapper logs. A retry is allowed
      // (this.adapter is still null), and the selective clear is idempotent
      // (mockNodeIds/mockProfileKeys are now empty, so filters are no-ops).
      throw err;
    }

    // Commit: assign adapter and register unlisteners.
    // (Selective clear was moved before listener registration per R3.)
    this.adapter = adapter;
    this.unlisteners.push(...localUnlisteners);
    } finally {
      this.connecting = false;
    }
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
    const { action, spaceId, kind, name, members, parentId, forkedFrom, pending } = payload;

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
        mentionCount: 0,
        unreadLevel: 'none',
        peer: undefined,
        // ZEB-285: carry fork lineage through to the NavNode so the
        // nav-tree can render the ↳ glyph + tooltip.
        forkedFrom,
        // ZEB-254: carry pending flag so greyed render shows immediately
        // when the invite-only join countersign hasn't arrived yet.
        pending: pending ?? undefined,
      };

      if (action === 'added') {
        const existing = this.nodes.find((n) => n.id === spaceId);
        if (existing) {
          // Preserve user-applied state on duplicate add (cold-replay):
          // parentId (folder placement), expanded, and unread counters.
          // Preserve forkedFrom from the new payload if provided (a
          // cold-replay "added" for a fork should carry lineage).
          this.nodes = this.nodes.map((n) =>
            n.id === spaceId
              ? {
                  ...newNode,
                  parentId: existing.parentId,
                  expanded: existing.expanded,
                  unreadCount: existing.unreadCount,
                  mentionCount: existing.mentionCount ?? 0,
                  unreadLevel: existing.unreadLevel,
                  forkedFrom: forkedFrom ?? existing.forkedFrom,
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
          // ZEB-254: pending: false clears the greyed state on joiner-side
          // countersign. Preserve existing pending when the payload doesn't
          // touch it (pending === undefined means "not relevant to this emit").
          const resolvedPending = pending === false ? false : (pending === true ? true : n.pending);
          // Preserve existing forkedFrom unless the payload supplies one.
          return { ...n, name, forkedFrom: forkedFrom ?? n.forkedFrom, pending: resolvedPending };
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
      mentionCount: 0,
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
          mentionCount: existing.mentionCount ?? 0,
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

  /** ZEB-662: walk parentId up to the owning community node id (or null). */
  private communityIdOf(node: NavNode): string | null {
    let cur: NavNode | undefined = node;
    const seen = new Set<string>();
    while (cur && !seen.has(cur.id)) {
      if (cur.type === 'community') return cur.id;
      seen.add(cur.id);
      cur = cur.parentId ? this.nodes.find((n) => n.id === cur!.parentId) : undefined;
    }
    return null;
  }

  /** ZEB-662: recompute a community node's mentionCount = sum of descendant
   *  channels' counts (channels whose ancestor community is this one). */
  private recomputeCommunityMentions(communityId: string): void {
    const comm = this.nodes.find((n) => n.id === communityId);
    if (!comm) return;
    let sum = 0;
    for (const n of this.nodes) {
      if (n.type === 'channel' && this.communityIdOf(n) === communityId) sum += n.mentionCount;
    }
    comm.mentionCount = sum;
  }

  /** ZEB-662: increment a channel's unseen-mention count and bubble to its community. */
  incMention(channelId: string): void {
    const node = this.nodes.find((n) => n.id === channelId);
    if (!node) return;
    node.mentionCount += 1;
    const cid = this.communityIdOf(node);
    if (cid) this.recomputeCommunityMentions(cid);
    this.onChange?.();
  }

  /** ZEB-662: clear a channel's mention count (channel opened) and re-bubble. */
  clearMention(channelId: string): void {
    const node = this.nodes.find((n) => n.id === channelId);
    if (!node || node.mentionCount === 0) return;
    node.mentionCount = 0;
    const cid = this.communityIdOf(node);
    if (cid) this.recomputeCommunityMentions(cid);
    this.onChange?.();
  }

  /**
   * ZEB-285: resolve the display name of a fork's parent community for
   * the fork-glyph tooltip. Returns the parent's name if the user is
   * still a member of the original (i.e. the original's NavNode is
   * present in this.nodes), or null if the parent isn't in the nav
   * (e.g. the user left the original via `also_leave` at fork time).
   */
  resolveForkParentName(originalId: string): string | null {
    const node = this.nodes.find((n) => n.id === originalId);
    return node?.name ?? null;
  }

  /**
   * ZEB-287 R3-1: resolve a community's display name from a hex SpaceId.
   * Used by `ForkLineageTree` to render a locally-known descendant fork
   * with its real name instead of a truncated hex string. Returns the
   * community's name if a `community`-kind NavNode is present, otherwise
   * null (e.g., the user isn't a member of that fork).
   */
  getCommunityNameBySpaceId(spaceId: string): string | null {
    const node = this.nodes.find(
      (n) => n.id === spaceId && n.type === 'community',
    );
    return node?.name ?? null;
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
