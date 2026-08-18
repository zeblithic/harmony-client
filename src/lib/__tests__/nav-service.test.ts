import { describe, it, expect, vi, beforeEach } from 'vitest';
import { NavService } from '../nav-service';
import type { TauriAdapter } from '../zenoh-service';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

const PEER = 'ef'.repeat(16); // 32-char peer hex

// ZEB-962: the `profile-update` handler bakes `wire.displayName` onto
// `NavNode.name` / `NavNode.peer.displayName` — the fields NavNodeRow (row text
// + Avatar) and the DM header render, WITHOUT the `resolveSpaceName` guard the
// sibling nav-updated path applies. A peer broadcasting a whitespace-only name
// would blank an already-resolved sidebar row. Route the update through the
// same ladder so a blank broadcast never degrades a resolved name.
describe('NavService profile-update name guard (ZEB-962)', () => {
  let service: NavService;
  let adapter: ReturnType<typeof makeAdapter>;

  beforeEach(() => {
    service = new NavService();
    adapter = makeAdapter();
  });

  async function seedDmNode(name: string) {
    await service.connectAdapter(adapter);
    const nav = adapter.listeners.get('nav-updated')!;
    nav({ payload: { action: 'added', spaceId: 'dm1', kind: 'dm', name, members: [PEER] } });
  }

  function profileUpdate(displayName: string) {
    const handler = adapter.listeners.get('profile-update')!;
    handler({ payload: { address: PEER, displayName, statusText: '' } });
    return service.nodes.find((n) => n.id === 'dm1');
  }

  it('applies a non-blank profile name to the node and peer', async () => {
    await seedDmNode('Placeholder');
    const node = profileUpdate('Real Name');
    expect(node?.name).toBe('Real Name');
    expect(node?.peer?.displayName).toBe('Real Name');
  });

  it('does NOT blank an already-resolved node name on a whitespace broadcast', async () => {
    await seedDmNode('Real Name');
    const node = profileUpdate('   ');
    expect(node?.name).toBe('Real Name');
    expect(node?.peer?.displayName).toBe('Real Name');
  });
});
