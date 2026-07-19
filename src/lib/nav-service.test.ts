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
            mentionCount: 0,
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
      { id: 'original-id', name: 'Cool Community', type: 'community', parentId: null, expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
      { id: 'fork-id', name: 'Cool Community (fork)', type: 'community', parentId: null, expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none', forkedFrom: 'original-id' },
    ];
    expect(svc.resolveForkParentName('original-id')).toBe('Cool Community');
  });

  it('returns null when forker is no longer a member of the original', () => {
    const svc = new NavService();
    // Only the fork is present — user left the original (alsoLeave=true).
    svc.nodes = [
      { id: 'fork-id', name: 'Cool Community (fork)', type: 'community', parentId: null, expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none', forkedFrom: 'original-id' },
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

describe('mention counts (ZEB-662)', () => {
  function svc(): NavService {
    const s = new NavService();
    s.nodes = [
      { id: 'c1', parentId: null, type: 'community', name: 'C', expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
      { id: 'ch1', parentId: 'c1', type: 'channel', name: 'general', expanded: false, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
      { id: 'ch2', parentId: 'c1', type: 'channel', name: 'random', expanded: false, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
    ];
    return s;
  }

  // Production has no channel nav nodes — only the community node.
  function prodSvc(): NavService {
    const s = new NavService();
    s.nodes = [
      { id: 'c1', parentId: null, type: 'community', name: 'C', expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
    ];
    return s;
  }

  it('dev-mock: incMention bumps the channel node and bubbles the community sum', () => {
    const s = svc();
    let changed = 0;
    s.onChange = () => {
      changed++;
    };
    s.incMention('c1', 'ch1');
    s.incMention('c1', 'ch1');
    s.incMention('c1', 'ch2');
    expect(s.nodes.find((n) => n.id === 'ch1')!.mentionCount).toBe(2);
    expect(s.nodes.find((n) => n.id === 'ch2')!.mentionCount).toBe(1);
    expect(s.nodes.find((n) => n.id === 'c1')!.mentionCount).toBe(3); // bubbled sum
    expect(changed).toBe(3);
  });

  it('dev-mock: clearMention zeroes the channel node and recomputes the community sum', () => {
    const s = svc();
    s.incMention('c1', 'ch1');
    s.incMention('c1', 'ch2');
    s.clearMention('ch1');
    expect(s.nodes.find((n) => n.id === 'ch1')!.mentionCount).toBe(0);
    expect(s.nodes.find((n) => n.id === 'c1')!.mentionCount).toBe(1); // only ch2 remains
  });

  it('boot race: a mention for a not-yet-materialized channel queues off the community bubble (ZEB-663)', () => {
    const s = prodSvc();
    const HLC = { wallMs: 0, logical: 0, deviceId: 'd' };
    const info = (channelId: string, name: string) => ({
      channelId,
      name,
      writePower: 0,
      kind: 'text' as const,
      createdAt: HLC,
    });
    // Channels aren't nav nodes yet (the boot gap before ChannelNavSync.start).
    // incMention queues by channelId rather than parking an unattributable
    // aggregate on the community node, so the community bubble stays clean.
    s.incMention('c1', 'ch-a');
    s.incMention('c1', 'ch-b');
    expect(s.nodes.find((n) => n.id === 'c1')!.mentionCount).toBe(0);
    // The queued mentions replay onto their channels once setChannels runs, so
    // a real unseen-mention badge is preserved (not lost, not stranded).
    s.setChannels('c1', [info('ch-a', 'general'), info('ch-b', 'random')]);
    expect(s.nodes.find((n) => n.id === 'ch-a')!.mentionCount).toBe(1);
    expect(s.nodes.find((n) => n.id === 'ch-b')!.mentionCount).toBe(1);
    expect(s.nodes.find((n) => n.id === 'c1')!.mentionCount).toBe(2); // bubbled sum
  });

  it('incMention with an unknown community and channel is a no-op', () => {
    const s = svc();
    s.incMention('nope-c', 'nope-ch');
    expect(s.nodes.find((n) => n.id === 'c1')!.mentionCount).toBe(0);
  });

  it('clearMention on an already-zero node does not fire onChange', () => {
    const s = svc();
    let changed = 0;
    s.onChange = () => {
      changed++;
    };
    s.clearMention('ch1');
    expect(changed).toBe(0);
  });
});

describe('NavService.setChannels reconcile (ZEB-663)', () => {
  const HLC = { wallMs: 0, logical: 0, deviceId: 'd' };
  const ch = (channelId: string, name: string, kind: 'text' | 'voice' = 'text') => ({
    channelId,
    name,
    writePower: 0,
    kind,
    createdAt: HLC,
  });

  function withCommunity(): NavService {
    const s = new NavService({ seedMockData: false });
    s.nodes = [
      { id: 'c1', parentId: null, type: 'community', name: 'C', expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
    ];
    return s;
  }

  it('adds channel children in listChannels order with channelKind', () => {
    const s = withCommunity();
    s.setChannels('c1', [ch('a', 'general'), ch('b', 'voice-room', 'voice')]);
    const kids = s.nodes.filter((n) => n.parentId === 'c1' && n.type === 'channel');
    expect(kids.map((n) => n.id)).toEqual(['a', 'b']);
    expect(kids.map((n) => n.name)).toEqual(['general', 'voice-room']);
    expect(kids.map((n) => n.channelKind)).toEqual(['text', 'voice']);
  });

  it('updates name and kind on survivors', () => {
    const s = withCommunity();
    s.setChannels('c1', [ch('a', 'general')]);
    s.setChannels('c1', [ch('a', 'lobby', 'voice')]);
    const a = s.nodes.find((n) => n.id === 'a')!;
    expect(a.name).toBe('lobby');
    expect(a.channelKind).toBe('voice');
  });

  it('preserves mentionCount + expanded on survivors across a reconcile', () => {
    const s = withCommunity();
    s.setChannels('c1', [ch('a', 'general'), ch('b', 'random')]);
    s.incMention('c1', 'a');
    s.incMention('c1', 'a');
    s.nodes.find((n) => n.id === 'a')!.expanded = true;
    s.setChannels('c1', [ch('a', 'general'), ch('b', 'random')]); // idempotent re-sync
    const a = s.nodes.find((n) => n.id === 'a')!;
    expect(a.mentionCount).toBe(2);
    expect(a.expanded).toBe(true);
    expect(s.nodes.find((n) => n.id === 'c1')!.mentionCount).toBe(2); // bubble intact
  });

  it('removes absent channels and subtracts their mentions from the community bubble', () => {
    const s = withCommunity();
    s.setChannels('c1', [ch('a', 'general'), ch('b', 'random')]);
    s.incMention('c1', 'a'); // community bubble = 1
    s.incMention('c1', 'b'); // community bubble = 2
    s.setChannels('c1', [ch('a', 'general')]); // b deleted
    expect(s.nodes.some((n) => n.id === 'b')).toBe(false);
    expect(s.nodes.find((n) => n.id === 'a')!.mentionCount).toBe(1);
    expect(s.nodes.find((n) => n.id === 'c1')!.mentionCount).toBe(1); // b's 1 subtracted
  });

  it('replays a boot-race mention onto its channel when channels materialize (ZEB-663)', () => {
    const s = withCommunity();
    // Boot race: a mention arrives for channel 'a' before its nav node exists.
    // incMention queues it by channelId (no unattributable parking on the
    // community), so the community bubble stays 0 until channels materialize.
    s.incMention('c1', 'a');
    expect(s.nodes.find((n) => n.id === 'c1')!.mentionCount).toBe(0);
    // Channels populate → the queued mention migrates onto 'a' and bubbles up.
    // The real unseen-mention badge is preserved, not wiped (was the
    // "mentions vanish after sync" regression) and not stranded on the community.
    s.setChannels('c1', [ch('a', 'general')]);
    expect(s.nodes.find((n) => n.id === 'a')!.mentionCount).toBe(1);
    expect(s.nodes.find((n) => n.id === 'c1')!.mentionCount).toBe(1);
  });

  it('a queued boot-race mention drains exactly once (survives a later idempotent resync)', () => {
    const s = withCommunity();
    s.incMention('c1', 'a');
    s.setChannels('c1', [ch('a', 'general')]); // materialize → replay onto 'a'
    s.setChannels('c1', [ch('a', 'general')]); // idempotent re-sync must not re-add
    expect(s.nodes.find((n) => n.id === 'a')!.mentionCount).toBe(1);
    expect(s.nodes.find((n) => n.id === 'c1')!.mentionCount).toBe(1);
  });

  it('reorders channel children (and re-renders) when only the order changes', () => {
    const s = withCommunity();
    let changed = 0;
    s.setChannels('c1', [ch('a', 'general'), ch('b', 'random')]);
    s.onChange = () => { changed++; };
    // Same id/name/kind set, different order → must rebuild + fire onChange,
    // because sibling render order follows array order among same-parent nodes.
    s.setChannels('c1', [ch('b', 'random'), ch('a', 'general')]);
    const kids = s.nodes.filter((n) => n.parentId === 'c1' && n.type === 'channel');
    expect(kids.map((n) => n.id)).toEqual(['b', 'a']);
    expect(changed).toBe(1);
  });

  it('fires onChange only when the reconcile changes the tree', () => {
    const s = withCommunity();
    let changed = 0;
    s.onChange = () => { changed++; };
    s.setChannels('c1', [ch('a', 'general')]); // add → 1
    s.setChannels('c1', [ch('a', 'general')]); // no-op → still 1
    expect(changed).toBe(1);
  });

  it('is a no-op when the community node is absent', () => {
    const s = new NavService({ seedMockData: false });
    s.nodes = [];
    let changed = 0;
    s.onChange = () => { changed++; };
    s.setChannels('nope', [ch('a', 'general')]);
    expect(s.nodes.length).toBe(0);
    expect(changed).toBe(0);
  });
});

describe('NavService community removal drops channel children (ZEB-663)', () => {
  it('removing a community also removes its channel nodes', () => {
    const s = new NavService({ seedMockData: false });
    s.nodes = [
      { id: 'c1', parentId: null, type: 'community', name: 'C', expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
    ];
    s.setChannels('c1', [
      { channelId: 'a', name: 'general', writePower: 0, kind: 'text', createdAt: { wallMs: 0, logical: 0, deviceId: 'd' } },
    ]);
    expect(s.nodes.some((n) => n.id === 'a')).toBe(true);
    s.addOrUpdateNavSpace({ action: 'removed', spaceId: 'c1', kind: 'community', name: 'C' });
    expect(s.nodes.some((n) => n.id === 'c1')).toBe(false);
    expect(s.nodes.some((n) => n.id === 'a')).toBe(false); // child dropped too
  });
});

describe('NavService — per-channel unread (ZEB-665)', () => {
  const HLC = { wallMs: 0, logical: 0, deviceId: 'd' };
  const info = (channelId: string, name: string) => ({
    channelId,
    name,
    writePower: 0,
    kind: 'text' as const,
    createdAt: HLC,
  });

  function seeded(): NavService {
    const s = new NavService({ seedMockData: false });
    s.nodes = [
      { id: 'c1', parentId: null, type: 'community', name: 'C', expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
    ];
    s.setChannels('c1', [info('ch1', 'general'), info('ch2', 'random')]);
    return s;
  }

  it('setUnread sets count + standard level and rolls up a quiet community dot', () => {
    const s = seeded();
    s.setUnread('ch1', 3);
    const ch1 = s.nodes.find((n) => n.id === 'ch1')!;
    const c1 = s.nodes.find((n) => n.id === 'c1')!;
    expect(ch1.unreadCount).toBe(3);
    expect(ch1.unreadLevel).toBe('standard');
    expect(c1.unreadCount).toBe(3);
    expect(c1.unreadLevel).toBe('quiet');
  });

  it('setUnread(0) clears the level and the rollup follows the sum', () => {
    const s = seeded();
    s.setUnread('ch1', 2);
    s.setUnread('ch2', 5);
    s.setUnread('ch1', 0);
    const ch1 = s.nodes.find((n) => n.id === 'ch1')!;
    const c1 = s.nodes.find((n) => n.id === 'c1')!;
    expect(ch1.unreadLevel).toBe('none');
    expect(c1.unreadCount).toBe(5);
    expect(c1.unreadLevel).toBe('quiet');
    s.setUnread('ch2', 0);
    expect(c1.unreadCount).toBe(0);
    expect(c1.unreadLevel).toBe('none');
  });

  it('setUnread on a missing node is a silent no-op', () => {
    const s = seeded();
    let changed = 0;
    s.onChange = () => { changed++; };
    expect(() => s.setUnread('nope', 4)).not.toThrow();
    expect(changed).toBe(0);
  });

  // ZEB-357: the third param carries the missed-call subset for DM rows.
  it('setUnread stores missedCalls on the node and notifies when only it changes', () => {
    const s = seeded();
    s.nodes = [
      ...s.nodes,
      { id: 'dm1', parentId: null, type: 'dm', name: 'Alice', expanded: false, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
    ];
    s.setUnread('dm1', 3, 2);
    const dm1 = s.nodes.find((n) => n.id === 'dm1')!;
    expect(dm1.unreadCount).toBe(3);
    expect(dm1.missedCallCount).toBe(2);
    let changed = 0;
    s.onChange = () => { changed++; };
    // Same unread, different missed — must still notify (not a no-op).
    s.setUnread('dm1', 3, 0);
    expect(changed).toBe(1);
    expect(s.nodes.find((n) => n.id === 'dm1')!.missedCallCount).toBe(0);
    // Fully identical call → no-op.
    s.setUnread('dm1', 3, 0);
    expect(changed).toBe(1);
  });

  it('setUnread without missedCalls leaves the missed count at 0 (channel callers)', () => {
    const s = seeded();
    s.setUnread('ch1', 3);
    expect(s.nodes.find((n) => n.id === 'ch1')!.missedCallCount ?? 0).toBe(0);
  });

  // PR #494 R1 (CodeRabbit): missedCallCount is documented as a subset of
  // unreadCount — clamp at the boundary so no caller can render a missed
  // call with zero unread messages.
  it('setUnread clamps missedCalls to the unread count', () => {
    const s = seeded();
    s.nodes = [
      ...s.nodes,
      { id: 'dm2', parentId: null, type: 'dm', name: 'Bob', expanded: false, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
    ];
    // Clamped to 0 == the all-zero no-op path: absent and 0 render identically.
    s.setUnread('dm2', 0, 1);
    expect(s.nodes.find((n) => n.id === 'dm2')!.missedCallCount ?? 0).toBe(0);
    s.setUnread('dm2', 2, 5);
    expect(s.nodes.find((n) => n.id === 'dm2')!.missedCallCount).toBe(2);
  });

  it('setUnread notifies onChange once per effective change and not on no-ops', () => {
    const s = seeded();
    let changed = 0;
    s.onChange = () => { changed++; };
    s.setUnread('ch1', 3);
    expect(changed).toBe(1);
    s.setUnread('ch1', 3); // same value — no-op
    expect(changed).toBe(1);
  });

  it('setUnread does not touch mention counts (parallel counters)', () => {
    const s = seeded();
    s.incMention('c1', 'ch1');
    s.setUnread('ch1', 4);
    const ch1 = s.nodes.find((n) => n.id === 'ch1')!;
    const c1 = s.nodes.find((n) => n.id === 'c1')!;
    expect(ch1.mentionCount).toBe(1);
    expect(c1.mentionCount).toBe(1);
    expect(ch1.unreadCount).toBe(4);
  });

  it('setChannels preserves per-channel unread and recomputes the rollup on removal', () => {
    const s = seeded();
    s.setUnread('ch1', 2);
    s.setUnread('ch2', 7);
    s.setChannels('c1', [info('ch1', 'general')]); // ch2 removed
    const ch1 = s.nodes.find((n) => n.id === 'ch1')!;
    const c1 = s.nodes.find((n) => n.id === 'c1')!;
    expect(ch1.unreadCount).toBe(2);
    expect(c1.unreadCount).toBe(2); // rollup follows survivors, ch2's 7 gone
    expect(c1.unreadLevel).toBe('quiet');
  });
});

describe('NavService — DM unread + space-change hook (ZEB-666)', () => {
  function withDm(): NavService {
    const s = new NavService({ seedMockData: false });
    s.addOrUpdateNavSpace({
      action: 'added', spaceId: 'dm1', kind: 'dm', name: 'DM with abcd',
      members: ['aaaa', 'bbbb'],
    });
    s.addOrUpdateNavSpace({
      action: 'added', spaceId: 'g1', kind: 'group-dm', name: 'Weekend crew',
      members: ['aaaa', 'bbbb', 'cccc'],
    });
    return s;
  }

  it('setUnread drives a dm node (count + standard level) and clears at 0', () => {
    const s = withDm();
    s.setUnread('dm1', 4);
    const dm = s.nodes.find((n) => n.id === 'dm1')!;
    expect(dm.unreadCount).toBe(4);
    expect(dm.unreadLevel).toBe('standard');
    s.setUnread('dm1', 0);
    expect(s.nodes.find((n) => n.id === 'dm1')!.unreadLevel).toBe('none');
  });

  it('setUnread drives a group-chat node', () => {
    const s = withDm();
    s.setUnread('g1', 2);
    const g = s.nodes.find((n) => n.id === 'g1')!;
    expect(g.unreadCount).toBe(2);
    expect(g.unreadLevel).toBe('standard');
  });

  it('setUnread on a DM node never touches community rollup', () => {
    const s = withDm();
    s.nodes = [
      ...s.nodes,
      { id: 'c1', parentId: null, type: 'community', name: 'C', expanded: true, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' },
    ];
    s.setUnread('dm1', 7);
    const c1 = s.nodes.find((n) => n.id === 'c1')!;
    expect(c1.unreadCount).toBe(0);
    expect(c1.unreadLevel).toBe('none');
  });

  it('onDmSpaceChange fires added for dm/group-dm adds and modifies', () => {
    const s = new NavService({ seedMockData: false });
    const events: Array<[string, string]> = [];
    s.onDmSpaceChange = (action, spaceId) => events.push([action, spaceId]);
    s.addOrUpdateNavSpace({ action: 'added', spaceId: 'dm1', kind: 'dm', name: 'D' });
    s.addOrUpdateNavSpace({ action: 'modified', spaceId: 'dm1', kind: 'dm', name: 'D2' });
    s.addOrUpdateNavSpace({ action: 'added', spaceId: 'g1', kind: 'group-dm', name: 'G' });
    expect(events).toEqual([
      ['added', 'dm1'],
      ['added', 'dm1'], // modified self-heals as added; seeding is idempotent
      ['added', 'g1'],
    ]);
  });

  it('onDmSpaceChange fires removed on dm removal', () => {
    const s = new NavService({ seedMockData: false });
    const events: Array<[string, string]> = [];
    s.addOrUpdateNavSpace({ action: 'added', spaceId: 'dm1', kind: 'dm', name: 'D' });
    s.onDmSpaceChange = (action, spaceId) => events.push([action, spaceId]);
    s.addOrUpdateNavSpace({ action: 'removed', spaceId: 'dm1', kind: 'dm', name: 'D' });
    expect(events).toEqual([['removed', 'dm1']]);
  });

  it('onDmSpaceChange does NOT fire for community payloads', () => {
    const s = new NavService({ seedMockData: false });
    const events: Array<[string, string]> = [];
    s.onDmSpaceChange = (action, spaceId) => events.push([action, spaceId]);
    s.addOrUpdateNavSpace({ action: 'added', spaceId: 'c1', kind: 'community', name: 'C' });
    s.addOrUpdateNavSpace({ action: 'removed', spaceId: 'c1', kind: 'community', name: 'C' });
    expect(events).toEqual([]);
  });
});
