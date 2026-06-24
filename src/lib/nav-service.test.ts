import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { NavService } from './nav-service';
import { navNodes as mockNavNodes, profileStore as mockProfileStore } from './mock-data';
import type { TauriAdapter } from './zenoh-service';

function createMockAdapter() {
  const listeners: Record<string, Array<(event: { payload: unknown }) => void>> = {};
  const adapter: TauriAdapter = {
    invoke: vi.fn().mockResolvedValue(undefined),
    listen: vi.fn().mockImplementation(async (event: string, handler: (event: { payload: unknown }) => void) => {
      if (!listeners[event]) listeners[event] = [];
      listeners[event].push(handler);
      return () => {
        listeners[event] = listeners[event].filter((h) => h !== handler);
      };
    }),
  };
  function emit(event: string, payload: unknown) {
    for (const handler of listeners[event] ?? []) {
      handler({ payload });
    }
  }
  return { adapter, emit, listeners };
}

// ZEB-560: the mock sidebar seed (mock community folders + friend DMs) must
// NOT render in the shipped/alpha GUI. The constructor gates seeding on
// `import.meta.env.DEV` (true under vitest / `vite dev`, false in a `vite
// build`), mirroring VineService's ZEB-546 fix — so a production build seeds
// nothing and there is no race-prone reliance on the end-of-boot clear-on-
// connect (which leaks the mock sidebar permanently if an earlier service
// connect in the boot chain stalls). These pin both modes deterministically.
describe('NavService mock-seed gating (ZEB-560)', () => {
  it('does NOT seed mock nav nodes/profiles when seedMockData is false (shipped/alpha build)', () => {
    const prod = new NavService({ seedMockData: false });
    expect(prod.nodes).toEqual([]);
    expect(prod.profiles.size).toBe(0);
  });

  it('seeds mock nav nodes/profiles when seedMockData is true (dev/browser)', () => {
    const dev = new NavService({ seedMockData: true });
    expect(dev.nodes.length).toBe(mockNavNodes.length);
    expect(dev.profiles.size).toBe(mockProfileStore.size);
  });

  it('a production-built service still ingests real nav-updated events after connect (no seed to clear)', async () => {
    const prod = new NavService({ seedMockData: false });
    const mock = createMockAdapter();
    await prod.connectAdapter(mock.adapter);
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'real-community-1',
      kind: 'community',
      name: 'Real Community',
    });
    expect(prod.nodes.find((n) => n.id === 'real-community-1')?.name).toBe('Real Community');
  });
});

describe('NavService DM handling', () => {
  let nav: NavService;
  let mock: ReturnType<typeof createMockAdapter>;

  beforeEach(async () => {
    nav = new NavService();
    mock = createMockAdapter();
    await nav.connectAdapter(mock.adapter);
  });

  afterEach(() => {
    nav.destroy();
  });

  it('inserts a top-level NavNode for a new DM Space via nav-updated', () => {
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'aabbccdd00112233',
      kind: 'dm',
      name: 'DM with Bob',
      members: ['bob-hex-address'],
      parentId: null,
    });

    expect(nav.nodes).toContainEqual(expect.objectContaining({
      id: 'aabbccdd00112233',
      type: 'dm',
      name: 'DM with Bob',
      parentId: null,
    }));
  });

  it('inserts a group-chat NavNode for a new GroupDm Space (kind=group-dm → type=group-chat)', () => {
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'eeff001122334455',
      kind: 'group-dm',
      name: 'Project Cabal',
      members: ['bob-hex', 'carol-hex', 'dave-hex'],
      parentId: null,
    });

    expect(nav.nodes).toContainEqual(expect.objectContaining({
      id: 'eeff001122334455',
      type: 'group-chat',
      name: 'Project Cabal',
      parentId: null,
    }));
  });

  it('removes a NavNode on nav-updated action=removed', () => {
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'doomed-space',
      kind: 'dm',
      name: 'DM with Eve',
      members: ['eve-hex'],
      parentId: null,
    });
    expect(nav.nodes.some((n) => n.id === 'doomed-space')).toBe(true);

    mock.emit('nav-updated', {
      action: 'removed',
      spaceId: 'doomed-space',
      kind: 'dm',
      name: '',
    });

    expect(nav.nodes.some((n) => n.id === 'doomed-space')).toBe(false);
  });

  it('updates an existing NavNode in place on nav-updated action=modified', () => {
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'rename-space',
      kind: 'dm',
      name: 'DM with Bob',
      members: ['bob-hex'],
      parentId: null,
    });

    mock.emit('nav-updated', {
      action: 'modified',
      spaceId: 'rename-space',
      kind: 'dm',
      name: 'Bob (renamed)',
      members: ['bob-hex'],
      parentId: null,
    });

    const node = nav.nodes.find((n) => n.id === 'rename-space');
    expect(node).toBeDefined();
    expect(node!.name).toBe('Bob (renamed)');
  });

  it('ignores nav-updated for non-DM kinds (channel/community/folder)', () => {
    const before = nav.nodes.length;
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'channel-space',
      kind: 'channel',
      name: '#general',
      parentId: null,
    });
    expect(nav.nodes.length).toBe(before);
    expect(nav.nodes.some((n) => n.id === 'channel-space')).toBe(false);
  });

  it('replaces an existing node on duplicate added (no double-insert)', () => {
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'dup-space',
      kind: 'dm',
      name: 'First',
      members: ['x-hex'],
      parentId: null,
    });
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'dup-space',
      kind: 'dm',
      name: 'Second',
      members: ['x-hex'],
      parentId: null,
    });

    const matches = nav.nodes.filter((n) => n.id === 'dup-space');
    expect(matches).toHaveLength(1);
    expect(matches[0].name).toBe('Second');
  });

  it('fires onChange when a DM nav-updated lands', () => {
    const onChange = vi.fn();
    nav.onChange = onChange;
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'change-space',
      kind: 'dm',
      name: 'DM with Bob',
      members: ['bob-hex'],
      parentId: null,
    });
    expect(onChange).toHaveBeenCalled();
  });

  it('attaches peer info for DM with a single member if profile is known', () => {
    nav.profiles.set('known-peer-hex', {
      address: 'known-peer-hex',
      displayName: 'Known Peer',
    });

    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'peer-space',
      kind: 'dm',
      name: 'DM with Known Peer',
      members: ['known-peer-hex'],
      parentId: null,
    });

    const node = nav.nodes.find((n) => n.id === 'peer-space');
    expect(node?.peer).toEqual({
      address: 'known-peer-hex',
      displayName: 'Known Peer',
    });
  });

  it('Fix F: peer = non-self member from a 2-member DM payload', () => {
    // Backend's add_space puts BOTH self and peer in `members` (sorted,
    // deduped). The frontend used to only attach a peer when
    // members.length === 1, which never matched the actual payload — so
    // 1:1 DMs never got a Profile attachment.
    nav.ownAddress = 'self-hex';
    nav.profiles.set('peer-hex', {
      address: 'peer-hex',
      displayName: 'Real Peer',
    });

    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'two-member-dm',
      kind: 'dm',
      name: 'DM with Real Peer',
      members: ['peer-hex', 'self-hex'],
      parentId: null,
    });

    const node = nav.nodes.find((n) => n.id === 'two-member-dm');
    expect(node?.peer?.address).toBe('peer-hex');
    expect(node?.peer?.displayName).toBe('Real Peer');
  });

  it('Fix F: falls back to members[0] when ownAddress not yet set', () => {
    nav.ownAddress = null;

    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'pre-bootstrap-dm',
      kind: 'dm',
      name: 'Pending DM',
      members: ['some-hex', 'other-hex'],
      parentId: null,
    });

    const node = nav.nodes.find((n) => n.id === 'pre-bootstrap-dm');
    // Either member is a defensible choice in the pre-bootstrap window.
    // We pick members[0] for determinism; a later profile-update will
    // refresh the peer attachment if needed.
    expect(node?.peer?.address).toBe('some-hex');
  });

  it('Fix F: group-dm attaches no peer regardless of member count', () => {
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'group-space',
      kind: 'group-dm',
      name: 'Group',
      members: ['a-hex', 'b-hex', 'c-hex'],
      parentId: null,
    });

    const node = nav.nodes.find((n) => n.id === 'group-space');
    expect(node?.peer).toBeUndefined();
  });

  it('Fix G: duplicate added preserves parentId/expanded/unread state', () => {
    // First insert.
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'persistent-dm',
      kind: 'dm',
      name: 'DM with Bob',
      members: ['bob-hex'],
      parentId: null,
    });

    // User folders it under 'family' and reads up some unread state.
    nav.nodes = nav.nodes.map((n) =>
      n.id === 'persistent-dm'
        ? {
            ...n,
            parentId: 'family-folder',
            expanded: true,
            unreadCount: 5,
            unreadLevel: 'standard',
          }
        : n,
    );

    // Reconnect / cold-start replay re-emits the same `added`.
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'persistent-dm',
      kind: 'dm',
      name: 'DM with Bob',
      members: ['bob-hex'],
      parentId: null, // backend has no concept of user-applied folder placement
    });

    const node = nav.nodes.find((n) => n.id === 'persistent-dm');
    expect(node?.parentId).toBe('family-folder');
    expect(node?.expanded).toBe(true);
    expect(node?.unreadCount).toBe(5);
    expect(node?.unreadLevel).toBe('standard');
  });
});

describe('NavService addOrUpdateNavSpace (direct call)', () => {
  // Fix B from PR #81 review: there's no Rust-side `nav-updated` emit
  // yet, so App.svelte's handleDmCreate calls addOrUpdateNavSpace
  // directly after add_space returns. The behavior must match the
  // listener's path (since a future backend emit could double-fire).
  let nav: NavService;

  beforeEach(() => {
    nav = new NavService();
    nav.nodes = [];
  });

  afterEach(() => {
    nav.destroy();
  });

  it('synthesizes a NavNode without an IPC emit', () => {
    nav.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'direct-space',
      kind: 'dm',
      name: 'DM with Carol',
      members: ['carol-hex'],
      parentId: null,
    });

    expect(nav.nodes).toHaveLength(1);
    expect(nav.nodes[0]).toEqual(expect.objectContaining({
      id: 'direct-space',
      type: 'dm',
      name: 'DM with Carol',
    }));
  });

  it('fires onChange for direct-call additions', () => {
    nav.onChange = vi.fn();
    nav.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'direct-space',
      kind: 'dm',
      name: 'DM with Dan',
      members: ['dan-hex'],
      parentId: null,
    });
    expect(nav.onChange).toHaveBeenCalled();
  });

  it('ignores non-DM kinds when called directly', () => {
    nav.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'channel-id',
      kind: 'channel',
      name: '#general',
      parentId: null,
    });
    expect(nav.nodes).toHaveLength(0);
  });
});

describe('NavService — community kind (ZEB-263)', () => {
  it('addOrUpdateNavSpace creates a community NavNode for kind: "community"', () => {
    const svc = new NavService();
    svc.nodes = []; // clear seeded mock data
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'aabbccdd' + 'ee'.repeat(28),
      kind: 'community',
      name: 'Test Crew',
      parentId: null,
    });

    expect(svc.nodes).toHaveLength(1);
    const node = svc.nodes[0];
    expect(node.type).toBe('community');
    expect(node.name).toBe('Test Crew');
    expect(node.parentId).toBeNull();
    expect(node.expanded).toBe(true);
    expect(node.peer).toBeUndefined();
  });

  it('addOrUpdateNavSpace silently ignores kind: "channel"', () => {
    const svc = new NavService();
    svc.nodes = [];
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'cc'.repeat(32),
      kind: 'channel',
      name: 'general',
      parentId: 'aabb' + 'cc'.repeat(28),
    });

    expect(svc.nodes).toHaveLength(0);
  });

  it('community node can have parentId set (placement inside user folder)', () => {
    const svc = new NavService();
    svc.nodes = [];
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'aabbccdd' + 'ee'.repeat(28),
      kind: 'community',
      name: 'Crew',
      parentId: 'folder-1',
    });

    expect(svc.nodes[0].parentId).toBe('folder-1');
  });

  it('removed action drops community node', () => {
    const svc = new NavService();
    svc.nodes = [];
    const id = 'aabbccdd' + 'ee'.repeat(28);
    svc.addOrUpdateNavSpace({ action: 'added', spaceId: id, kind: 'community', name: 'Crew' });
    expect(svc.nodes).toHaveLength(1);
    svc.addOrUpdateNavSpace({ action: 'removed', spaceId: id, kind: 'community', name: 'Crew' });
    expect(svc.nodes).toHaveLength(0);
  });

  it('existing dm/group-dm path still works unchanged (regression)', () => {
    const svc = new NavService();
    svc.nodes = [];
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'dd'.repeat(32),
      kind: 'dm',
      name: 'Bob',
      members: ['bob_addr', 'self_addr'],
    });

    expect(svc.nodes).toHaveLength(1);
    expect(svc.nodes[0].type).toBe('dm');
  });

  it('Fix-G analog: duplicate added preserves parentId/expanded/unread state for community', () => {
    const svc = new NavService();
    svc.nodes = [];
    const spaceId = 'aabbccdd' + 'ee'.repeat(28);

    // First insert with parentId='folder-1'.
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId,
      kind: 'community',
      name: 'Original Name',
      parentId: 'folder-1',
    });

    // Simulate user-applied UI state: collapse the node and give it unread counts.
    svc.nodes = svc.nodes.map((n) =>
      n.id === spaceId
        ? { ...n, expanded: false, unreadCount: 3 }
        : n,
    );

    // Cold-replay re-emits added with parentId=null (backend has no concept of
    // user-applied folder placement or unread state).
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId,
      kind: 'community',
      name: 'New Name',
      parentId: null,
    });

    const node = svc.nodes.find((n) => n.id === spaceId);
    expect(node).toBeDefined();
    // Preserved from existing user state.
    expect(node!.parentId).toBe('folder-1');
    expect(node!.expanded).toBe(false);
    expect(node!.unreadCount).toBe(3);
    // Name updated from the replay payload.
    expect(node!.name).toBe('New Name');
  });
});

describe('NavService — community kind via nav-updated listener (ZEB-265)', () => {
  // ZEB-265 wires backend emits from create_community / redeem_invite /
  // leave_community. Once the listener path is live, App.svelte stops
  // synthesizing community NavNodes locally — these tests cover the
  // listener path that replaces the synthesis.
  //
  // ZEB-209: connectAdapter now clears mock-seeded state, so the
  // explicit `nav.nodes = []` below is redundant but kept for clarity.
  let nav: NavService;
  let mock: ReturnType<typeof createMockAdapter>;

  beforeEach(async () => {
    nav = new NavService();
    nav.nodes = [];
    mock = createMockAdapter();
    await nav.connectAdapter(mock.adapter);
  });

  afterEach(() => {
    nav.destroy();
  });

  it('create_community emit: listener inserts a community NavNode', () => {
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: 'aabbccdd' + 'ee'.repeat(28),
      kind: 'community',
      name: 'Created via emit',
      parentId: null,
    });

    expect(nav.nodes).toContainEqual(expect.objectContaining({
      id: 'aabbccdd' + 'ee'.repeat(28),
      type: 'community',
      name: 'Created via emit',
      parentId: null,
    }));
  });

  it('redeem_invite emit: listener inserts a community NavNode with the invite name', () => {
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: '11'.repeat(16),
      kind: 'community',
      name: 'Redeemed Crew',
      parentId: null,
    });

    const node = nav.nodes.find((n) => n.id === '11'.repeat(16));
    expect(node?.type).toBe('community');
    expect(node?.name).toBe('Redeemed Crew');
  });

  it('leave_community emit: listener removes the community NavNode', () => {
    const id = '22'.repeat(16);
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: id,
      kind: 'community',
      name: 'Doomed Community',
      parentId: null,
    });
    expect(nav.nodes.some((n) => n.id === id)).toBe(true);

    mock.emit('nav-updated', {
      action: 'removed',
      spaceId: id,
      kind: 'community',
      name: '',
    });
    expect(nav.nodes.some((n) => n.id === id)).toBe(false);
  });

  it('community emit fires onChange so the UI re-renders', () => {
    const onChange = vi.fn();
    nav.onChange = onChange;
    mock.emit('nav-updated', {
      action: 'added',
      spaceId: '33'.repeat(16),
      kind: 'community',
      name: 'Re-render check',
      parentId: null,
    });
    expect(onChange).toHaveBeenCalled();
  });
});

describe('NavService mock-clear policy (ZEB-209)', () => {
  it('clears mock-seeded nodes and profiles on connectAdapter', async () => {
    const nav = new NavService();
    // Sanity: constructor seeds from mockNavNodes + mockProfileStore so
    // the UI is never empty in browser/dev mode (no adapter connects).
    expect(nav.nodes.length).toBeGreaterThan(0);
    expect(nav.profiles.size).toBeGreaterThan(0);
    const { adapter } = createMockAdapter();
    await nav.connectAdapter(adapter);
    expect(nav.nodes).toEqual([]);
    expect(nav.profiles.size).toBe(0);
    nav.destroy();
  });

  it('fires onChange once after clearing mocks', async () => {
    const nav = new NavService();
    let calls = 0;
    nav.onChange = () => { calls++; };
    const { adapter } = createMockAdapter();
    await nav.connectAdapter(adapter);
    expect(calls).toBeGreaterThanOrEqual(1);
    nav.destroy();
  });
});

describe('NavService.resolveForkParentName (ZEB-285)', () => {
  it('returns parent community name when forker is still a member', () => {
    const svc = new NavService();
    // Replace mock-seeded nodes with controlled fixture.
    svc.nodes = [
      { id: 'original-id', name: 'Cool Community', type: 'community', parentId: null, expanded: true, unreadCount: 0, unreadLevel: 'none' },
      { id: 'fork-id', name: 'Cool Community (fork)', type: 'community', parentId: null, expanded: true, unreadCount: 0, unreadLevel: 'none', forkedFrom: 'original-id' },
    ];
    expect(svc.resolveForkParentName('original-id')).toBe('Cool Community');
  });

  it('returns null when forker is no longer a member of the original', () => {
    const svc = new NavService();
    // Only the fork is present — user left the original (alsoLeave=true).
    svc.nodes = [
      { id: 'fork-id', name: 'Cool Community (fork)', type: 'community', parentId: null, expanded: true, unreadCount: 0, unreadLevel: 'none', forkedFrom: 'original-id' },
    ];
    expect(svc.resolveForkParentName('original-id')).toBe(null);
  });
});

describe('NavService — pending community (ZEB-254)', () => {
  it('community with pending=true in payload creates a NavNode with pending: true', () => {
    const svc = new NavService();
    svc.nodes = [];
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'pending-community-id',
      kind: 'community',
      name: 'Secret Crew',
      parentId: null,
      pending: true,
    });

    const node = svc.nodes.find((n) => n.id === 'pending-community-id');
    expect(node).toBeDefined();
    expect(node!.pending).toBe(true);
  });

  it('community without pending field creates a NavNode with pending: undefined', () => {
    const svc = new NavService();
    svc.nodes = [];
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'open-community-id',
      kind: 'community',
      name: 'Open Crew',
      parentId: null,
    });

    const node = svc.nodes.find((n) => n.id === 'open-community-id');
    expect(node).toBeDefined();
    expect(node!.pending).toBeUndefined();
  });

  it('nav-updated { action: modified, pending: false } clears pending state', async () => {
    const svc = new NavService();
    svc.nodes = [];

    // Start as pending.
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'pending-community-id',
      kind: 'community',
      name: 'Secret Crew',
      parentId: null,
      pending: true,
    });
    expect(svc.nodes.find((n) => n.id === 'pending-community-id')!.pending).toBe(true);

    // Countersign arrives — backend emits modified { pending: false }.
    svc.addOrUpdateNavSpace({
      action: 'modified',
      spaceId: 'pending-community-id',
      kind: 'community',
      name: 'Secret Crew',
      pending: false,
    });

    const node = svc.nodes.find((n) => n.id === 'pending-community-id');
    expect(node).toBeDefined();
    expect(node!.pending).toBe(false);
  });

  it('nav-updated { action: modified } without pending field preserves existing pending', () => {
    const svc = new NavService();
    svc.nodes = [];

    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'pending-community-id',
      kind: 'community',
      name: 'Secret Crew',
      parentId: null,
      pending: true,
    });

    // A name-only modified (no pending field) should not clear pending.
    svc.addOrUpdateNavSpace({
      action: 'modified',
      spaceId: 'pending-community-id',
      kind: 'community',
      name: 'Secret Crew (renamed)',
    });

    const node = svc.nodes.find((n) => n.id === 'pending-community-id');
    expect(node!.pending).toBe(true);
    expect(node!.name).toBe('Secret Crew (renamed)');
  });

  it('nav-updated modified pending=false fires onChange', () => {
    const svc = new NavService();
    svc.nodes = [];
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'p-id',
      kind: 'community',
      name: 'Crew',
      pending: true,
    });

    const onChange = vi.fn();
    svc.onChange = onChange;
    svc.addOrUpdateNavSpace({
      action: 'modified',
      spaceId: 'p-id',
      kind: 'community',
      name: 'Crew',
      pending: false,
    });

    expect(onChange).toHaveBeenCalled();
  });

  it('nav-updated listener: pending=false via event ungreys the node', async () => {
    const svc = new NavService();
    svc.nodes = [];
    const mock = createMockAdapter();
    await svc.connectAdapter(mock.adapter);

    // Seed a pending community directly (simulating a synthesized nav node
    // added before the adapter wired in).
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'listen-pending-id',
      kind: 'community',
      name: 'Invite Crew',
      pending: true,
    });
    expect(svc.nodes.find((n) => n.id === 'listen-pending-id')!.pending).toBe(true);

    // Backend fires nav-updated { pending: false } when countersign lands.
    mock.emit('nav-updated', {
      action: 'modified',
      spaceId: 'listen-pending-id',
      kind: 'community',
      name: 'Invite Crew',
      pending: false,
    });

    const node = svc.nodes.find((n) => n.id === 'listen-pending-id');
    expect(node!.pending).toBe(false);
    svc.destroy();
  });
});
