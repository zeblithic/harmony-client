import { describe, it, expect, vi, beforeEach } from 'vitest';
import { VineService, type VineDescriptorEvent } from './vine-service';
import { vineVideos as mockVines } from './mock-data';
import { createMockAdapter } from './test-utils';
import type { VineVideo } from './types';

describe('VineService', () => {
  let svc: VineService;

  beforeEach(() => {
    svc = new VineService();
  });

  // ── Constructor ───────────────────────────────────────────────────

  it('seeds with mock vines', () => {
    expect(svc.vines.length).toBe(mockVines.length);
  });

  it('populates viewedIds from mock data', () => {
    const expectedViewed = mockVines.filter(v => v.viewed).map(v => v.id);
    for (const id of expectedViewed) {
      expect(svc.viewedIds.has(id)).toBe(true);
    }
  });

  it('populates seenIds so mock vines are deduped', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const existingId = mockVines[0].id;
    emit('vine-received', {
      id: existingId, creatorAddress: 'x', creatorName: 'X',
      createdAt: 1, videoCid: 'cid-dup',
    } satisfies VineDescriptorEvent);
    expect(svc.vines.length).toBe(mockVines.length);
  });

  // ── connectAdapter ────────────────────────────────────────────────

  it('registers a vine-received listener', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    expect(adapter.listen).toHaveBeenCalledWith('vine-received', expect.any(Function));
  });

  it('idempotent — second call is a no-op', async () => {
    const { adapter: a1 } = createMockAdapter();
    const { adapter: a2 } = createMockAdapter();
    await svc.connectAdapter(a1);
    await svc.connectAdapter(a2);
    expect(a2.listen).not.toHaveBeenCalled();
  });

  it('appends incoming wire vines', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const wire: VineDescriptorEvent = {
      id: 'net-v1', creatorAddress: 'abc123', creatorName: 'Peer',
      createdAt: 1700000500, videoCid: 'cid-new',
    };
    emit('vine-received', wire);
    expect(svc.vines.length).toBe(mockVines.length + 1);
    expect(svc.vines.at(-1)!.videoCid).toBe('cid-new');
  });

  it('deduplicates by id', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const wire: VineDescriptorEvent = {
      id: 'dup-v1', creatorAddress: 'x', creatorName: 'X',
      createdAt: 1, videoCid: 'cid-x',
    };
    emit('vine-received', wire);
    emit('vine-received', wire);
    expect(svc.vines.filter(v => v.id === 'dup-v1').length).toBe(1);
  });

  it('calls onChange when a new vine arrives', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.onChange = vi.fn();
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'notify-v1', creatorAddress: 'x', creatorName: 'X',
      createdAt: 1, videoCid: 'cid-y',
    } satisfies VineDescriptorEvent);
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  // ── wireToVine ────────────────────────────────────────────────────

  it('maps self-published vines to address "self"', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.ownAddress = 'myaddr';
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'self-v1', creatorAddress: 'myaddr', creatorName: 'Me',
      createdAt: 1, videoCid: 'cid-self',
    } satisfies VineDescriptorEvent);
    const vine = svc.vines.find(v => v.id === 'self-v1')!;
    expect(vine.creatorAddress).toBe('self');
    expect(vine.creatorName).toBe('You');
    expect(vine.viewed).toBe(true);
  });

  it('marks self-published vines as viewed automatically', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.ownAddress = 'myaddr';
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'self-v2', creatorAddress: 'myaddr', creatorName: 'Me',
      createdAt: 1, videoCid: 'cid-self2',
    } satisfies VineDescriptorEvent);
    expect(svc.viewedIds.has('self-v2')).toBe(true);
  });

  it('falls back to truncated address when creatorName is empty', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'noname-v1', creatorAddress: 'abcdef1234567890', creatorName: '',
      createdAt: 1, videoCid: 'cid-z',
    } satisfies VineDescriptorEvent);
    const vine = svc.vines.find(v => v.id === 'noname-v1')!;
    expect(vine.creatorName).toBe('abcdef12');
  });

  it('preserves optional fields (title, reshareOf)', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'opt-v1', creatorAddress: 'x', creatorName: 'X',
      createdAt: 1, videoCid: 'cid-opt', title: 'My Vine', reshareOf: 'orig-1',
    } satisfies VineDescriptorEvent);
    const vine = svc.vines.find(v => v.id === 'opt-v1')!;
    expect(vine.title).toBe('My Vine');
    expect(vine.reshareOf).toBe('orig-1');
  });

  it('preserves original creator attribution on incoming wire vines', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'vine-resh',
      creatorAddress: 'addr-resharer',
      creatorName: 'Resharer',
      createdAt: 1,
      videoCid: 'cid-r',
      reshareOf: 'orig-1',
      originalCreatorAddress: 'addr-original',
      originalCreatorName: 'Original',
    } satisfies VineDescriptorEvent);
    const vine = svc.vines.find(v => v.id === 'vine-resh')!;
    expect(vine.reshareOf).toBe('orig-1');
    expect(vine.originalCreatorAddress).toBe('addr-original');
    expect(vine.originalCreatorName).toBe('Original');
  });

  // ── publish ────────────────────────────────────────────────────────

  it('invokes publish_vine on the adapter', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    await svc.publish('cid-pub', 'Title', 'reshare-of-1');
    expect(adapter.invoke).toHaveBeenCalledWith('publish_vine', {
      vine: { videoCid: 'cid-pub', title: 'Title', reshareOf: 'reshare-of-1', creatorName: 'You' },
    });
  });

  it('publish forwards original creator fields to adapter', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    await svc.publish('cid-pub', 'Title', 'reshare-of-1', 'addr-original', 'Original');
    expect(adapter.invoke).toHaveBeenCalledWith('publish_vine', {
      vine: {
        videoCid: 'cid-pub',
        title: 'Title',
        reshareOf: 'reshare-of-1',
        creatorName: 'You',
        originalCreatorAddress: 'addr-original',
        originalCreatorName: 'Original',
      },
    });
  });

  it('publish omits original creator fields when not provided', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    await svc.publish('cid-pub', 'Title');
    // Vitest's toEqual treats {a:1, b:undefined} as equal to {a:1},
    // so omitting these keys here also matches when publish passes
    // them as undefined.
    expect(adapter.invoke).toHaveBeenCalledWith('publish_vine', {
      vine: {
        videoCid: 'cid-pub',
        title: 'Title',
        reshareOf: undefined,
        creatorName: 'You',
        originalCreatorAddress: undefined,
        originalCreatorName: undefined,
      },
    });
  });

  it('falls back to local vine when no adapter', async () => {
    const before = svc.vines.length;
    await svc.publish('cid-offline', 'Offline Vine');
    expect(svc.vines.length).toBe(before + 1);
    const vine = svc.vines.at(-1)!;
    expect(vine.videoCid).toBe('cid-offline');
    expect(vine.creatorAddress).toBe('self');
    expect(vine.viewed).toBe(true);
  });

  it('auto-marks offline-published vine as viewed', async () => {
    await svc.publish('cid-offv');
    const vine = svc.vines.at(-1)!;
    expect(svc.viewedIds.has(vine.id)).toBe(true);
  });

  it('falls back locally on "not connected" error', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('not connected'));
    await svc.connectAdapter(adapter);
    const before = svc.vines.length;
    await svc.publish('cid-fallback');
    expect(svc.vines.length).toBe(before + 1);
  });

  it('re-throws non-connectivity errors', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('invalid CID format'));
    await svc.connectAdapter(adapter);
    await expect(svc.publish('bad-cid')).rejects.toThrow('invalid CID format');
  });

  it('calls onChange on offline publish', async () => {
    svc.onChange = vi.fn();
    await svc.publish('cid-local');
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  // ── markViewed ─────────────────────────────────────────────────────

  it('adds id to viewedIds', () => {
    const unviewedId = mockVines.find(v => !v.viewed)?.id;
    expect(unviewedId).toBeDefined();
    svc.markViewed(unviewedId!);
    expect(svc.viewedIds.has(unviewedId!)).toBe(true);
  });

  it('is idempotent — no double onChange', () => {
    const id = mockVines.find(v => !v.viewed)!.id;
    svc.onChange = vi.fn();
    svc.markViewed(id);
    svc.markViewed(id);
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  it('invokes mark_vine_viewed on the adapter', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const id = mockVines.find(v => !v.viewed)!.id;
    svc.markViewed(id);
    expect(adapter.invoke).toHaveBeenCalledWith('mark_vine_viewed', { vineId: id });
  });

  it('calls onChange on markViewed', () => {
    svc.onChange = vi.fn();
    const id = mockVines.find(v => !v.viewed)!.id;
    svc.markViewed(id);
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  // ── destroy / addUnlisten ─────────────────────────────────────────

  it('destroy calls all registered unlisteners', async () => {
    const { adapter, unlisten } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const external = vi.fn();
    svc.addUnlisten(external);
    svc.destroy();
    // Two adapter listeners registered (vine-received + vine-reaction-received)
    expect(unlisten).toHaveBeenCalledTimes(2);
    expect(external).toHaveBeenCalledOnce();
  });

  it('destroy is safe to call twice', async () => {
    const { adapter, unlisten } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.destroy();
    svc.destroy();
    // Two adapter listeners registered (vine-received + vine-reaction-received)
    expect(unlisten).toHaveBeenCalledTimes(2);
  });

  // ── Follow / feed routing ──────────────────────────────────────────

  it('routes "followed" vines to followedVines', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'fv-1', creatorAddress: 'aabb', creatorName: 'Alice',
      createdAt: 1, videoCid: 'cid-f1', source: 'followed',
    });
    expect(svc.followedVines.length).toBe(1);
    expect(svc.discoverVines.length).toBe(mockVines.length);
  });

  it('routes "discover" vines to discoverVines', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'dv-1', creatorAddress: 'ccdd', creatorName: 'Bob',
      createdAt: 1, videoCid: 'cid-d1', source: 'discover',
    });
    expect(svc.discoverVines.length).toBe(mockVines.length + 1);
    expect(svc.followedVines.length).toBe(0);
  });

  it('treats vines without source as discover', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'ns-1', creatorAddress: 'eeff', creatorName: 'Carol',
      createdAt: 1, videoCid: 'cid-ns',
    });
    expect(svc.discoverVines.length).toBe(mockVines.length + 1);
  });

  it('follow moves existing vines from discover to followed', async () => {
    const { adapter, emit } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    await svc.connectAdapter(adapter);
    emit('vine-received', {
      id: 'mv-1', creatorAddress: 'aabb', creatorName: 'Alice',
      createdAt: 1, videoCid: 'cid-mv1', source: 'discover',
    });
    expect(svc.discoverVines.length).toBe(mockVines.length + 1);
    await svc.follow('aabb', 'Alice');
    expect(svc.discoverVines.find(v => v.id === 'mv-1')).toBeFalsy();
    expect(svc.followedVines.find(v => v.id === 'mv-1')).toBeTruthy();
  });

  it('follow calls follow_creator Tauri command', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    await svc.connectAdapter(adapter);
    await svc.follow('aabb', 'Alice');
    expect(adapter.invoke).toHaveBeenCalledWith('follow_vine_creator', {
      address: 'aabb', name: 'Alice',
    });
  });

  it('unfollow moves vines from followedVines back to discoverVines', async () => {
    const { adapter, emit } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    await svc.connectAdapter(adapter);
    svc.followedAddresses.add('aabb');
    emit('vine-received', {
      id: 'uf-1', creatorAddress: 'aabb', creatorName: 'Alice',
      createdAt: 1, videoCid: 'cid-uf1', source: 'followed',
    });
    const discoverBefore = svc.discoverVines.length;
    await svc.unfollow('aabb');
    expect(svc.followedVines.length).toBe(0);
    expect(svc.followedAddresses.has('aabb')).toBe(false);
    expect(svc.discoverVines.find(v => v.id === 'uf-1')).toBeTruthy();
    expect(svc.discoverVines.length).toBe(discoverBefore + 1);
  });

  it('unfollow calls unfollow_vine_creator Tauri command', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    await svc.connectAdapter(adapter);
    svc.followedAddresses.add('aabb');
    await svc.unfollow('aabb');
    expect(adapter.invoke).toHaveBeenCalledWith('unfollow_vine_creator', {
      address: 'aabb',
    });
  });

  it('isFollowed checks local cache', () => {
    svc.followedAddresses.add('aabb');
    expect(svc.isFollowed('aabb')).toBe(true);
    expect(svc.isFollowed('ccdd')).toBe(false);
  });

  // ── Reactions ──────────────────────────────────────────────────────

  it('getReaction returns zero state for unknown vine', () => {
    const r = svc.getReaction('nonexistent');
    expect(r.count).toBe(0);
    expect(r.likedByMe).toBe(false);
  });

  it('toggleLike optimistically sets likedByMe and increments count', async () => {
    const vine = mockVines[0];
    svc.onChange = vi.fn();
    await svc.toggleLike(vine);
    const r = svc.getReaction(vine.id);
    expect(r.likedByMe).toBe(true);
    expect(r.count).toBe(1);
    expect(svc.onChange).toHaveBeenCalled();
  });

  it('toggleLike again unlikes and decrements count', async () => {
    const vine = mockVines[0];
    await svc.toggleLike(vine);
    await svc.toggleLike(vine);
    const r = svc.getReaction(vine.id);
    expect(r.likedByMe).toBe(false);
    expect(r.count).toBe(0);
  });

  it('toggleLike calls publish_vine_reaction on adapter', async () => {
    const { adapter } = createMockAdapter();
    svc.ownAddress = 'myaddr';
    await svc.connectAdapter(adapter);
    const vine = mockVines[0];
    await svc.toggleLike(vine);
    expect(adapter.invoke).toHaveBeenCalledWith('publish_vine_reaction', {
      reaction: {
        vineId: vine.id,
        vineCreatorAddress: vine.creatorAddress,
        liked: true,
        reactorName: 'You',
      },
    });
  });

  it('toggleLike resolves self address for own vines', async () => {
    const { adapter } = createMockAdapter();
    svc.ownAddress = 'myaddr';
    await svc.connectAdapter(adapter);
    const selfVine = { ...mockVines[0], creatorAddress: 'self' };
    await svc.toggleLike(selfVine);
    expect(adapter.invoke).toHaveBeenCalledWith('publish_vine_reaction', {
      reaction: {
        vineId: selfVine.id,
        vineCreatorAddress: 'myaddr',
        liked: true,
        reactorName: 'You',
      },
    });
  });

  it('toggleLike skips publish when ownAddress is null', async () => {
    const { adapter } = createMockAdapter();
    // ownAddress is null (not yet fetched)
    await svc.connectAdapter(adapter);
    const vine = mockVines[0];
    await svc.toggleLike(vine);
    // Optimistic update should still apply
    expect(svc.getReaction(vine.id).likedByMe).toBe(true);
    // But no publish should have been sent
    expect(adapter.invoke).not.toHaveBeenCalledWith('publish_vine_reaction', expect.anything());
  });

  it('toggleLike migrates stale self entry when ownAddress becomes available', async () => {
    // Like offline (ownAddress is null) — stores 'self' in reactors
    const vine = mockVines[0];
    await svc.toggleLike(vine);
    expect(svc.getReaction(vine.id).likedByMe).toBe(true);
    // Now ownAddress gets set
    svc.ownAddress = 'myaddr';
    // Unlike — should migrate 'self' to 'myaddr' and correctly decrement
    await svc.toggleLike(vine);
    expect(svc.getReaction(vine.id).likedByMe).toBe(false);
    expect(svc.getReaction(vine.id).count).toBe(0);
  });

  it('toggleLike works offline without adapter', async () => {
    const vine = mockVines[0];
    await svc.toggleLike(vine);
    expect(svc.getReaction(vine.id).likedByMe).toBe(true);
  });

  it('toggleLike rolls back on adapter error', async () => {
    const { adapter } = createMockAdapter();
    svc.ownAddress = 'myaddr';
    (adapter.invoke as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('network error'));
    await svc.connectAdapter(adapter);
    const vine = mockVines[0];
    await svc.toggleLike(vine);
    const r = svc.getReaction(vine.id);
    expect(r.likedByMe).toBe(false);
    expect(r.count).toBe(0);
  });

  it('loadFollowed populates followedAddresses', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'list_followed') {
        return Promise.resolve([
          { address: 'aabb', name: 'Alice', followedAt: 1 },
          { address: 'ccdd', name: null, followedAt: 2 },
        ]);
      }
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);
    await svc.loadFollowed();
    expect(svc.followedAddresses.has('aabb')).toBe(true);
    expect(svc.followedAddresses.has('ccdd')).toBe(true);
  });

  it('incoming reaction increments count', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-reaction-received', {
      vineId: mockVines[0].id,
      reactorAddress: 'peer-abc',
      reactorName: 'Peer',
      liked: true,
      timestamp: 1700000500,
    });
    const r = svc.getReaction(mockVines[0].id);
    expect(r.count).toBe(1);
    expect(r.likedByMe).toBe(false);
  });

  it('incoming reaction deduplicates by reactor address', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const event = {
      vineId: mockVines[0].id,
      reactorAddress: 'peer-abc',
      reactorName: 'Peer',
      liked: true,
      timestamp: 1700000500,
    };
    emit('vine-reaction-received', event);
    emit('vine-reaction-received', event);
    expect(svc.getReaction(mockVines[0].id).count).toBe(1);
  });

  it('incoming unlike decrements count', async () => {
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('vine-reaction-received', {
      vineId: mockVines[0].id,
      reactorAddress: 'peer-abc',
      reactorName: 'Peer',
      liked: true,
      timestamp: 1700000500,
    });
    emit('vine-reaction-received', {
      vineId: mockVines[0].id,
      reactorAddress: 'peer-abc',
      reactorName: 'Peer',
      liked: false,
      timestamp: 1700000600,
    });
    expect(svc.getReaction(mockVines[0].id).count).toBe(0);
  });

  it('skips self-echo reactions', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.ownAddress = 'myaddr';
    await svc.connectAdapter(adapter);
    const vine = mockVines[0];
    await svc.toggleLike(vine);
    expect(svc.getReaction(vine.id).count).toBe(1);
    emit('vine-reaction-received', {
      vineId: vine.id,
      reactorAddress: 'myaddr',
      reactorName: 'You',
      liked: true,
      timestamp: 1700000500,
    });
    expect(svc.getReaction(vine.id).count).toBe(1);
  });

  it('ignores reactions for unknown vine IDs', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.onChange = vi.fn();
    await svc.connectAdapter(adapter);
    (svc.onChange as ReturnType<typeof vi.fn>).mockClear();
    emit('vine-reaction-received', {
      vineId: 'nonexistent-vine',
      reactorAddress: 'peer-abc',
      reactorName: 'Peer',
      liked: true,
      timestamp: 1700000500,
    });
    expect(svc.onChange).not.toHaveBeenCalled();
  });

  it('calls onChange when reaction arrives', async () => {
    const { adapter, emit } = createMockAdapter();
    svc.onChange = vi.fn();
    await svc.connectAdapter(adapter);
    (svc.onChange as ReturnType<typeof vi.fn>).mockClear();
    emit('vine-reaction-received', {
      vineId: mockVines[0].id,
      reactorAddress: 'peer-xyz',
      reactorName: 'Peer',
      liked: true,
      timestamp: 1700000500,
    });
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  it('reconciles misrouted vines after loadFollowed', async () => {
    const { adapter, emit } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'list_followed') {
        return Promise.resolve([
          { address: 'aabb', name: 'Alice', followedAt: 1 },
        ]);
      }
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);
    // Vine arrives before loadFollowed — routes to discover
    emit('vine-received', {
      id: 'recon-1', creatorAddress: 'aabb', creatorName: 'Alice',
      createdAt: 1, videoCid: 'cid-r1', source: 'discover',
    });
    expect(svc.discoverVines.find(v => v.id === 'recon-1')).toBeTruthy();
    // Now load follows — should move to followed
    await svc.loadFollowed();
    expect(svc.discoverVines.find(v => v.id === 'recon-1')).toBeFalsy();
    expect(svc.followedVines.find(v => v.id === 'recon-1')).toBeTruthy();
  });
});

describe('VineService.findVine', () => {
  let svc: VineService;

  beforeEach(() => {
    svc = new VineService();
  });

  it('returns vine from followedVines by id', () => {
    const vine: VineVideo = {
      id: 'vine-f', creatorAddress: 'a', creatorName: 'A',
      createdAt: 1, videoCid: 'cid', viewed: false,
    };
    svc.followedVines = [vine];
    expect(svc.findVine('vine-f')).toBe(vine);
  });

  it('returns vine from discoverVines by id', () => {
    const vine: VineVideo = {
      id: 'vine-d', creatorAddress: 'a', creatorName: 'A',
      createdAt: 1, videoCid: 'cid', viewed: false,
    };
    svc.discoverVines = [vine];
    expect(svc.findVine('vine-d')).toBe(vine);
  });

  it('returns undefined when no vine matches', () => {
    expect(svc.findVine('nonexistent-id')).toBeUndefined();
  });

  it('searches followedVines before discoverVines (order tie-break)', () => {
    // Same id in both feeds shouldn't happen, but if it does, followed wins.
    const fVine: VineVideo = {
      id: 'dup', creatorAddress: 'a', creatorName: 'F',
      createdAt: 1, videoCid: 'cid', viewed: false,
    };
    const dVine: VineVideo = { ...fVine, creatorName: 'D' };
    svc.followedVines = [fVine];
    svc.discoverVines = [dVine];
    expect(svc.findVine('dup')).toBe(fVine);
  });
});

describe('VineService.getReshareCount', () => {
  let svc: VineService;

  beforeEach(() => {
    svc = new VineService();
  });

  it('returns 0 when no vines reshare the id', () => {
    expect(svc.getReshareCount('vine-none')).toBe(0);
  });

  it('counts reshares across both feeds', () => {
    const origId = 'vine-orig';
    const r1: VineVideo = {
      id: 'r1', creatorAddress: 'a', creatorName: 'A',
      createdAt: 1, videoCid: 'c', viewed: false, reshareOf: origId,
    };
    const r2: VineVideo = {
      id: 'r2', creatorAddress: 'b', creatorName: 'B',
      createdAt: 1, videoCid: 'c', viewed: false, reshareOf: origId,
    };
    const r3: VineVideo = {
      id: 'r3', creatorAddress: 'c', creatorName: 'C',
      createdAt: 1, videoCid: 'c', viewed: false, reshareOf: origId,
    };
    svc.followedVines = [r1, r2];
    svc.discoverVines = [r3];
    expect(svc.getReshareCount(origId)).toBe(3);
  });

  it('does not count vines that are not reshares', () => {
    const orig: VineVideo = {
      id: 'vine-orig', creatorAddress: 'a', creatorName: 'A',
      createdAt: 1, videoCid: 'c', viewed: false,
    };
    svc.followedVines = [orig];
    expect(svc.getReshareCount('vine-orig')).toBe(0);
  });

  it('does not count reshares of a different id', () => {
    const reshareOfOther: VineVideo = {
      id: 'r1', creatorAddress: 'a', creatorName: 'A',
      createdAt: 1, videoCid: 'c', viewed: false, reshareOf: 'different-orig',
    };
    svc.followedVines = [reshareOfOther];
    expect(svc.getReshareCount('target-orig')).toBe(0);
  });
});

// ── publish: self-reshare prevention ────────────────────────────────
//
// Belt-and-suspenders backstop: the UI hides the reshare button on
// own-origin vines (Task 6), but the guard here ensures a stale or
// programmatic caller cannot accidentally publish a self-reshare. The
// caller (App.svelte::handleVineReshare) is responsible for resolving
// the true chain origin before calling publish — the guard only checks
// `originalCreatorAddress` against the local identity.
describe('VineService self-reshare prevention', () => {
  let svc: VineService;

  beforeEach(() => {
    svc = new VineService();
  });

  it('publish silently no-ops when resharing own original (creatorAddress === "self")', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const before = svc.vines.length;
    // Reshare with originalCreatorAddress === 'self' should be rejected silently.
    await svc.publish('cid-x', 'My title', 'orig-1', 'self', 'You');
    expect(adapter.invoke).not.toHaveBeenCalledWith('publish_vine', expect.anything());
    expect(svc.vines.length).toBe(before);
  });

  it('publish silently no-ops when originalCreatorAddress === ownAddress (hex form)', async () => {
    const { adapter } = createMockAdapter();
    svc.ownAddress = 'a1b2c3d4';
    await svc.connectAdapter(adapter);
    const before = svc.vines.length;
    await svc.publish('cid-x', 'My title', 'orig-1', 'a1b2c3d4', 'You');
    expect(adapter.invoke).not.toHaveBeenCalledWith('publish_vine', expect.anything());
    expect(svc.vines.length).toBe(before);
  });

  it("publish allows resharing someone else's reshare of your content", async () => {
    // Carol resharing Bob's reshare of Alice's vine. The originalCreator
    // *of the vine Carol is resharing* is Bob, even though Carol's content
    // ultimately traces to Alice's original. But the spec says: trace to
    // true origin. So if true origin === self, no-op. Here true origin is
    // someone else, so it goes through.
    //
    // The implementer's responsibility: the GUARD checks
    // originalCreatorAddress against ownAddress/'self'. The CALLER
    // (App.svelte handleVineReshare) is responsible for resolving the
    // true origin before calling publish.
    const { adapter } = createMockAdapter();
    svc.ownAddress = 'self-addr';
    await svc.connectAdapter(adapter);
    await svc.publish('cid-x', 'Title', 'reshare-of-1', 'other-addr', 'Other');
    expect(adapter.invoke).toHaveBeenCalledWith('publish_vine', expect.anything());
  });

  it('publish allows non-reshare originals (no reshareOf, no original creator fields)', async () => {
    const { adapter } = createMockAdapter();
    svc.ownAddress = 'self-addr';
    await svc.connectAdapter(adapter);
    await svc.publish('cid-x', 'Original title');
    expect(adapter.invoke).toHaveBeenCalledWith('publish_vine', expect.anything());
  });
});
