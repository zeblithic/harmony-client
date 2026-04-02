import { describe, it, expect, vi, beforeEach } from 'vitest';
import { VineService, type VineDescriptorEvent } from './vine-service';
import type { TauriAdapter } from './zenoh-service';
import { vineVideos as mockVines } from './mock-data';

function createMockAdapter() {
  const listeners = new Map<string, (event: { payload: unknown }) => void>();
  const unlisten = vi.fn();
  const adapter: TauriAdapter = {
    invoke: vi.fn().mockResolvedValue(undefined),
    listen: vi.fn().mockImplementation((event: string, handler: (event: { payload: unknown }) => void) => {
      listeners.set(event, handler);
      return Promise.resolve(unlisten);
    }),
  };
  function emit(event: string, payload: unknown) {
    listeners.get(event)?.({ payload });
  }
  return { adapter, emit, unlisten };
}

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

  // ── publish ────────────────────────────────────────────────────────

  it('invokes publish_vine on the adapter', async () => {
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    await svc.publish('cid-pub', 'Title', 'reshare-of-1');
    expect(adapter.invoke).toHaveBeenCalledWith('publish_vine', {
      vine: { videoCid: 'cid-pub', title: 'Title', reshareOf: 'reshare-of-1', creatorName: 'You' },
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
    expect(unlisten).toHaveBeenCalledOnce();
    expect(external).toHaveBeenCalledOnce();
  });

  it('destroy is safe to call twice', async () => {
    const { adapter, unlisten } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.destroy();
    svc.destroy();
    expect(unlisten).toHaveBeenCalledOnce();
  });
});
