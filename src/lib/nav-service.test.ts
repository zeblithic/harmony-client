import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { NavService } from './nav-service';
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
});
