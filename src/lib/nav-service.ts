import type { TauriAdapter, ProfilePayload } from './zenoh-service';
import type { NavNode, NavNodeType, Profile } from './types';
import type { ChannelInfo } from './community-service';
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
 * ZEB-839 — does `name` look like a raw-address placeholder rather than a name
 * a human would recognise?
 *
 * The backend has no profile for a DM peer when it mints the Space, so every
 * DM-kind `nav-updated` / `list_owner_dm_spaces` row carries a stand-in built
 * from the raw owner hex (`"DM with <ownerHex>"` — `lib.rs:64329`,
 * `dm_outbox.rs:2556`), and the comment at the emit site says outright that
 * "the frontend replaces it with the resolved peer profile name". Cold-replay
 * of those rows must therefore not overwrite a name we already resolved.
 *
 * Matches: empty/whitespace; the address itself; any name CONTAINING the
 * address (the `"DM with <hex>"` shape); and any name containing a hex run of
 * ≥6 chars that prefixes the address (the truncated-hex idiom used across the
 * UI). A real display name reaching those last two would have to be a long hex
 * prefix of this specific peer's owner_id.
 */
function isAddressPlaceholder(name: string, address?: string): boolean {
  const trimmed = name.trim();
  if (trimmed === '') return true;
  if (!address) return false;
  const lower = trimmed.toLowerCase();
  const addr = address.toLowerCase();
  if (lower.includes(addr)) return true;
  return (lower.match(/[0-9a-f]{6,}/g) ?? []).some((run) => addr.startsWith(run));
}

/** ZEB-839 — the name to render for a dm/group-dm node. Live peer profile wins;
 *  then the incoming payload name, unless it is a raw-address placeholder and
 *  we already hold a resolved one; hex placeholder only as the last resort. */
function resolveSpaceName(
  payloadName: string,
  existingName: string | undefined,
  profileName: string | undefined,
  peerAddress: string | undefined,
): string {
  if (profileName !== undefined && profileName.trim() !== '') return profileName;
  if (!isAddressPlaceholder(payloadName, peerAddress)) return payloadName;
  if (existingName !== undefined && !isAddressPlaceholder(existingName, peerAddress)) {
    return existingName;
  }
  return payloadName;
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
  /** ZEB-666: fired from `addOrUpdateNavSpace`'s dm/group-dm path so
   *  DmUnreadService can seed ('added' — also fired for modified, which
   *  self-heals as added; seeding is idempotent) or drop ('removed')
   *  per-thread unread state. Assign BEFORE connectAdapter (an
   *  optional-chained hook is a silent no-op; nothing replays it). */
  onDmSpaceChange?: (action: 'added' | 'removed', spaceId: string) => void;

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
  /** ZEB-663: boot-race mentions, keyed by `channelId`, for channels whose nav
   *  nodes don't exist yet. `incMention` queues here; `setChannels` drains onto
   *  the channel when it materializes (see those methods). Entries for channels
   *  that never materialize are harmless and bounded by distinct channelIds
   *  mentioned during a boot gap. */
  private pendingChannelMentions = new Map<string, number>();

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
        // ZEB-663: a community's channel children are removed with it.
        this.nodes = this.nodes.filter((n) => n.id !== spaceId && n.parentId !== spaceId);
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
      this.onDmSpaceChange?.('removed', spaceId);
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
    // ZEB-839: the laddered name goes on `node.name` — the field the sidebar row
    // (NavNodeRow) and the DM header (via App.svelte's activeChannelName)
    // actually render. It used to land only on `node.peer.displayName`, which
    // only the Avatar reads, so the row kept showing the backend's raw-hex
    // placeholder even with a resolved profile in hand. `existing` is resolved
    // up front because the ladder's third rung is the name we already hold.
    const existing = this.nodes.find((n) => n.id === spaceId);
    // A name-only re-emit carries no `members`, so `peerAddress` is undefined on
    // that path — fall back to the peer we already hold so both the profile rung
    // and the placeholder guard still apply. (`peer` below is deliberately NOT
    // built from this: whether to keep the cached peer is the modified/Fix-G
    // contract, unchanged here.)
    const namePeerAddress = peerAddress ?? existing?.peer?.address;
    const peerProfile = namePeerAddress ? this.profiles.get(namePeerAddress) : undefined;
    const resolvedName = resolveSpaceName(
      name,
      existing?.name,
      peerProfile?.displayName,
      namePeerAddress,
    );
    const peer = peerAddress
      ? {
          address: peerAddress,
          displayName: resolvedName,
          avatarUrl: peerProfile?.avatarUrl,
        }
      : undefined;
    const newNode: NavNode = {
      id: spaceId,
      type: navType,
      name: resolvedName,
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
      //
      // ZEB-839: `resolvedName` (not the raw payload `name`) — a re-sync
      // carrying the backend's raw-address placeholder must not clobber a name
      // the `profile-update` handler already resolved.
      const peerWasDerivedFromPayload = members !== undefined;
      let found = false;
      this.nodes = this.nodes.map((n) => {
        if (n.id !== spaceId) return n;
        found = true;
        return { ...n, name: resolvedName, peer: peerWasDerivedFromPayload ? peer : n.peer };
      });
      if (!found) {
        // Modified for an unknown spaceId — treat as added to stay
        // self-healing on missed `added` events.
        this.nodes = [...this.nodes, newNode];
      }
    }
    this.onChange?.();
    this.onDmSpaceChange?.('added', spaceId);
  }

  /**
   * ZEB-663: reconcile a community's channel children against a live
   * (deleted-filtered) `ChannelInfo[]`. Rebuilds the child block in
   * `listChannels` order (backend sorts created_at asc, general-first),
   * preserving each survivor's `mentionCount`/`expanded`. Removed channels'
   * mentions are subtracted from the community bubble so the sum invariant
   * holds. Fires `onChange()` once iff the tree actually changed (an
   * idempotent re-sync is silent, so a community switch doesn't re-render).
   */
  setChannels(communityId: string, channels: ChannelInfo[]): void {
    const community = this.nodes.find(
      (n) => n.id === communityId && n.type === 'community',
    );
    if (!community) return; // no community node to attach to → nothing to do

    const prev = new Map(
      this.nodes
        .filter((n) => n.parentId === communityId && n.type === 'channel')
        .map((n) => [n.id, n] as const),
    );

    // Detect structural change: a differing id set, any per-channel name/kind
    // change, or a pure reorder. Order is load-bearing — sibling render order
    // follows array order among same-parent nodes (channels have no
    // `lastActivity`, so `sortNodes('activity')` is a stable no-op) — so a
    // reordered-but-otherwise-identical list must still rebuild + re-render.
    // `prev` is built by filtering `this.nodes` in order, so its key order is
    // the current render order.
    const prevOrder = [...prev.keys()];
    let changed = prevOrder.length !== channels.length;
    if (!changed) {
      for (let i = 0; i < channels.length; i++) {
        const info = channels[i];
        const old = prev.get(info.channelId);
        if (
          !old ||
          old.name !== info.name ||
          old.channelKind !== info.kind ||
          prevOrder[i] !== info.channelId
        ) {
          changed = true;
          break;
        }
      }
    }

    // A pending boot-race mention for any materializing channel must be
    // replayed even if the channel set is otherwise idempotent, so don't
    // early-return while there's queued work for an incoming channel.
    const hasPendingReplay = channels.some((c) =>
      this.pendingChannelMentions.has(c.channelId),
    );

    if (!changed && !hasPendingReplay) return; // idempotent re-sync — stay silent

    // Rebuild the child block in listChannels order, preserving live state.
    // A channel materializing for the first time drains any boot-race mentions
    // that `incMention` queued for it while its nav node didn't exist yet — so
    // a real unseen-mention badge migrates onto the correct channel instead of
    // being lost or stranded on the community (`old` and `pending` are mutually
    // exclusive: once a channel is a node, incMention targets it directly and
    // never queues).
    const nextChildren: NavNode[] = channels.map((info) => {
      const old = prev.get(info.channelId);
      const pending = this.pendingChannelMentions.get(info.channelId) ?? 0;
      if (pending) this.pendingChannelMentions.delete(info.channelId);
      return {
        id: info.channelId,
        parentId: communityId,
        type: 'channel',
        channelKind: info.kind,
        name: info.name,
        expanded: old?.expanded ?? false,
        unreadCount: old?.unreadCount ?? 0,
        mentionCount: (old?.mentionCount ?? 0) + pending,
        unreadLevel: old?.unreadLevel ?? 'none',
      };
    });

    // Replace only this community's channel children; keep every other node's
    // order. Insert the block right after the community node so children stay
    // grouped (sibling render order is by array order among same-parent nodes).
    const others = this.nodes.filter(
      (n) => !(n.parentId === communityId && n.type === 'channel'),
    );
    const communityIdx = others.findIndex((n) => n.id === communityId);
    this.nodes = [
      ...others.slice(0, communityIdx + 1),
      ...nextChildren,
      ...others.slice(communityIdx + 1),
    ];

    // ZEB-663: enforce the sum invariant — the community bubble equals the sum
    // of its channels' mention counts. Besides handling removals, this
    // self-heals a boot-race stray: a mention that landed on the community node
    // (channel-else-community fallback) before its channels were populated
    // reconciles to the children sum the moment channels appear, instead of
    // stranding a phantom badge that nothing can clear (the ZEB-662 community-
    // open clear was removed because it zeroed legit child-bubbled counts too).
    community.mentionCount = nextChildren.reduce((sum, c) => sum + c.mentionCount, 0);
    // ZEB-665: same sum invariant for unread (a removal drops its child count).
    this.rollUpCommunityUnread(communityId);

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

  /** ZEB-662: apply a signed delta to a channel's mentionCount and mirror it on
   *  the owning community node (incremental — O(tree depth), not a full scan).
   *  The community node's count stays == the sum of its channels' counts because
   *  inc/clear are the only mutators and each applies the same delta to both. */
  private applyMentionDelta(channelId: string, delta: number): void {
    const node = this.nodes.find((n) => n.id === channelId);
    if (!node) return;
    const next = Math.max(0, node.mentionCount + delta);
    const applied = next - node.mentionCount;
    if (applied === 0) return;
    node.mentionCount = next;
    const cid = this.communityIdOf(node);
    if (cid && cid !== node.id) {
      const comm = this.nodes.find((n) => n.id === cid);
      if (comm) comm.mentionCount = Math.max(0, comm.mentionCount + applied);
    }
    this.onChange?.();
  }

  /** ZEB-662/663: increment an unseen-mention count for a community channel.
   *  When the channel's nav node exists, bump it and bubble to its community.
   *  When it doesn't yet — the boot-race window before `ChannelNavSync` has
   *  materialized this community's channels — queue the delta by `channelId`
   *  so `setChannels` can replay it onto the right channel the moment it
   *  appears. Queuing (rather than parking an unattributable count on the
   *  community node) is what lets `setChannels` hold `community == Σ(children)`
   *  without either losing the mention or stranding a phantom badge. */
  incMention(communityId: string, channelId: string): void {
    if (this.nodes.some((n) => n.id === channelId)) {
      this.applyMentionDelta(channelId, 1);
      return;
    }
    // Channel node absent. Only queue for a community we're actually in — a
    // stray for an unknown community is dropped (matches the prior no-op).
    if (!this.nodes.some((n) => n.id === communityId && n.type === 'community')) {
      return;
    }
    this.pendingChannelMentions.set(
      channelId,
      (this.pendingChannelMentions.get(channelId) ?? 0) + 1,
    );
  }

  /** ZEB-662: clear a node's mention count (its channel/DM opened, or — in
   *  production — its community opened) and re-bubble. Accepts either a channel
   *  node id (dev/mock) or a community node id (production aggregate). */
  clearMention(id: string): void {
    const node = this.nodes.find((n) => n.id === id);
    if (!node || node.mentionCount === 0) return;
    this.applyMentionDelta(id, -node.mentionCount);
  }

  /** ZEB-665/ZEB-666: absolute per-node unread count (from the unread
   *  services' capped ID-sets — recomputable projections, so no boot-race
   *  queue like mentions: the services re-push when nodes materialize).
   *  Drives channel, dm, and group-chat nodes; only channels roll the
   *  owning community up to Σ(children) with a `quiet` dot (numbers on
   *  channels, dot on the community — deliberate messenger idiom; DM rows
   *  have no aggregation target). */
  setUnread(channelId: string, count: number, missedCalls: number = 0): void {
    const node = this.nodes.find(
      (n) =>
        n.id === channelId &&
        (n.type === 'channel' || n.type === 'dm' || n.type === 'group-chat'),
    );
    if (!node) return;
    const next = Math.max(0, count);
    // ZEB-357: DM/group-chat rows carry the missed-call subset as a distinct
    // 📞 badge. Channel callers omit the param (defaults 0) — no rollup, DM
    // rows have no aggregation target (same as unreadCount). Clamped to the
    // unread count: missed is documented as a subset, and no caller may render
    // a missed call with zero unread messages (PR #494 R1).
    const nextMissed = Math.min(next, Math.max(0, missedCalls));
    const nextLevel: NavNode['unreadLevel'] = next > 0 ? 'standard' : 'none';
    if (
      node.unreadCount === next &&
      node.unreadLevel === nextLevel &&
      (node.missedCallCount ?? 0) === nextMissed
    ) {
      return;
    }
    const delta = next - node.unreadCount;
    node.unreadCount = next;
    node.missedCallCount = nextMissed;
    node.unreadLevel = nextLevel;
    // Incremental community rollup (the applyMentionDelta idiom) — setUnread
    // sits on the per-message hot path, so no full-node scan here. setChannels
    // still full-recomputes via rollUpCommunityUnread when structure changes.
    // Channel nodes only: a DM's parentId is a user folder, not a community.
    if (node.type === 'channel') {
      const cid = this.communityIdOf(node);
      if (cid) {
        const comm = this.nodes.find((n) => n.id === cid);
        if (comm) {
          comm.unreadCount = Math.max(0, comm.unreadCount + delta);
          comm.unreadLevel = comm.unreadCount > 0 ? 'quiet' : 'none';
        }
      }
    }
    this.onChange?.();
  }

  /** ZEB-665: community unread = Σ(channel children); level quiet/none. */
  private rollUpCommunityUnread(communityId: string): void {
    const comm = this.nodes.find((n) => n.id === communityId && n.type === 'community');
    if (!comm) return;
    const sum = this.nodes
      .filter((n) => n.parentId === communityId && n.type === 'channel')
      .reduce((acc, c) => acc + c.unreadCount, 0);
    comm.unreadCount = sum;
    comm.unreadLevel = sum > 0 ? 'quiet' : 'none';
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
