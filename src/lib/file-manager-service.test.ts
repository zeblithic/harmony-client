import { describe, it, expect, vi } from 'vitest';
import {
  FileManagerService,
  inferCategory,
  unreadReceivedCount,
  type ContentAnnouncementEvent,
} from './file-manager-service';
import { createMockAdapter } from './test-utils';
import type { FileGrant, ReceivedFile } from './types';

describe('FileManagerService', () => {
  it('constructs with default settings', () => {
    const svc = new FileManagerService();
    expect(svc.settings.defaultReplicationTier).toBe('default');
  });

  it('constructs with custom settings', () => {
    const svc = new FileManagerService({ defaultReplicationTier: 'high' });
    expect(svc.settings.defaultReplicationTier).toBe('high');
  });

  it('returns private content', () => {
    const svc = new FileManagerService();
    const items = svc.getContents();
    expect(items.length).toBeGreaterThan(0);
    // All items should have a cid
    for (const item of items) {
      expect(item.cid).toBeTruthy();
    }
  });

  it('filters content by parentCid null for root items', () => {
    const svc = new FileManagerService();
    const root = svc.getContents(null);
    for (const item of root) {
      expect(item.parentCid).toBeNull();
    }
    // Should include both folders and files at root
    expect(root.length).toBeGreaterThan(0);
  });

  it('filters content by specific parentCid for children', () => {
    const svc = new FileManagerService();
    const children = svc.getContents('cid-folder-projects');
    expect(children.length).toBeGreaterThan(0);
    for (const item of children) {
      expect(item.parentCid).toBe('cid-folder-projects');
    }
  });

  it('returns empty array for parentCid with no children', () => {
    const svc = new FileManagerService();
    const children = svc.getContents('nonexistent-cid');
    expect(children).toEqual([]);
  });

  it('returns quota status with real used bytes and no invented total (ZEB-612 S3)', () => {
    const svc = new FileManagerService();
    const quota = svc.getQuotaStatus();
    expect(quota.usedBytes).toBeGreaterThan(0);
    // byCategory should have entries
    expect(Object.keys(quota.byCategory).length).toBeGreaterThan(0);
    // Demo mode (no adapter): the real pinned budget is unknown.
    expect(quota.pinnedBudgetBytes).toBeNull();
    expect('totalBytes' in quota).toBe(false);
  });

  // ── ZEB-612 S3: real replicaCount + pinned budget ──────────────────

  const wireItem = (over: Record<string, unknown> = {}) => ({
    sidecarId: 'sc-1',
    cid: 'cid-real-1',
    name: 'real.txt',
    sizeBytes: 1000,
    storedAt: 1_700_000_000_000,
    sensitivity: 'private',
    replicationTier: 'default',
    pinned: false,
    licensed: false,
    archived: false,
    kind: 'leaf',
    replicaCount: 1,
    ...over,
  });

  function adapterWith(routes: Record<string, unknown | (() => Promise<unknown>)>) {
    const { adapter, emit } = createMockAdapter();
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd in routes) {
        const r = routes[cmd];
        return typeof r === 'function' ? (r as () => Promise<unknown>)() : Promise.resolve(r);
      }
      return Promise.resolve(undefined);
    });
    return { adapter, emit };
  }

  it('maps wire replicaCount instead of fabricating 1', async () => {
    const svc = new FileManagerService();
    const { adapter } = adapterWith({ list_content: [wireItem({ replicaCount: 4 })] });
    await svc.connectAdapter(adapter);
    expect(svc.getContents()[0].replicaCount).toBe(4);
  });

  // ── ZEB-669 S3/S4: backup flag + origin provenance ──────────────────

  it('maps wire backup and origin, defaulting false/null when absent', async () => {
    const svc = new FileManagerService();
    const { adapter } = adapterWith({
      list_content: [
        wireItem({
          sidecarId: 'sc-b',
          cid: 'cid-b',
          backup: true,
          origin: { kind: 'selfIngest', introducer: null },
        }),
        wireItem({ sidecarId: 'sc-legacy', cid: 'cid-legacy' }),
      ],
    });
    await svc.connectAdapter(adapter);
    const byId = new Map(svc.getContents().map((i) => [i.sidecarId, i]));
    expect(byId.get('sc-b')!.backup).toBe(true);
    expect(byId.get('sc-b')!.origin).toEqual({ kind: 'selfIngest', introducer: null });
    expect(byId.get('sc-legacy')!.backup).toBe(false);
    expect(byId.get('sc-legacy')!.origin).toBeNull();
  });

  it('setBackupFlag invokes set_backup_flag and updates local state', async () => {
    const svc = new FileManagerService();
    const { adapter } = adapterWith({
      list_content: [wireItem({ sidecarId: 'sc-b', cid: 'cid-b', backup: false })],
      set_backup_flag: undefined,
    });
    await svc.connectAdapter(adapter);
    const onChange = vi.fn();
    svc.onChange = onChange;

    await svc.setBackupFlag('sc-b', true);
    expect(adapter.invoke).toHaveBeenCalledWith('set_backup_flag', {
      sidecarId: 'sc-b',
      backup: true,
    });
    expect(svc.getContents()[0].backup).toBe(true);
    expect(onChange).toHaveBeenCalled();
  });

  it('getContentDetail prefers the exact sidecar row among duplicate-CID siblings', async () => {
    const svc = new FileManagerService();
    const { adapter } = adapterWith({
      list_content: [
        wireItem({ sidecarId: 'sc-1', cid: 'cid-shared', backup: false }),
        wireItem({ sidecarId: 'sc-2', cid: 'cid-shared', backup: true }),
      ],
    });
    await svc.connectAdapter(adapter);
    // CID-only lookup lands on the first sibling — the sidecarId hint must win.
    expect(svc.getContentDetail('cid-shared', 'sc-2')!.backup).toBe(true);
    expect(svc.getContentDetail('cid-shared', 'sc-1')!.backup).toBe(false);
    // Fallback path (manifest rows / legacy callers) still works.
    expect(svc.getContentDetail('cid-shared')!.sidecarId).toBe('sc-1');
  });

  it('refreshContents re-fetches list_content and fires onChange', async () => {
    const svc = new FileManagerService();
    let rows = [wireItem({ sidecarId: 'sc-1', cid: 'cid-1', backup: false })];
    const { adapter } = adapterWith({ list_content: () => Promise.resolve(rows) });
    await svc.connectAdapter(adapter);
    expect(svc.getContents()[0].backup).toBe(false);

    rows = [wireItem({ sidecarId: 'sc-1', cid: 'cid-1', backup: true })];
    const onChange = vi.fn();
    svc.onChange = onChange;
    await svc.refreshContents();
    expect(svc.getContents()[0].backup).toBe(true);
    expect(onChange).toHaveBeenCalled();
  });

  it('setBackupFlag propagates the ineligible rejection without mutating state', async () => {
    const svc = new FileManagerService();
    const { adapter } = adapterWith({
      list_content: [wireItem({ sidecarId: 'sc-b', cid: 'cid-b', backup: false })],
      set_backup_flag: () => Promise.reject('ineligible: content class is not public durable'),
    });
    await svc.connectAdapter(adapter);

    await expect(svc.setBackupFlag('sc-b', true)).rejects.toThrow(
      'ineligible: content class is not public durable'
    );
    expect(svc.getContents()[0].backup).toBe(false);
  });

  it('fetches the pinned budget on connect and exposes it in quota status', async () => {
    const svc = new FileManagerService();
    const { adapter } = adapterWith({
      list_content: [],
      get_storage_budget: { cacheCapacity: 512, maxPinnedBytes: 50_000_000 },
    });
    await svc.connectAdapter(adapter);
    expect(adapter.invoke).toHaveBeenCalledWith('get_storage_budget');
    expect(svc.getQuotaStatus().pinnedBudgetBytes).toBe(50_000_000);
  });

  it('computes pinned usage from pinned items only, deduped by cid', async () => {
    const svc = new FileManagerService();
    const { adapter } = adapterWith({
      list_content: [
        wireItem({ sidecarId: 'a', cid: 'c-pin', sizeBytes: 1000, pinned: true }),
        // Symlink-style second sidecar on the same CID — counts once.
        wireItem({ sidecarId: 'b', cid: 'c-pin', sizeBytes: 1000, pinned: true }),
        wireItem({ sidecarId: 'c', cid: 'c-loose', sizeBytes: 2000 }),
      ],
      get_storage_budget: { cacheCapacity: 512, maxPinnedBytes: 50_000_000 },
    });
    await svc.connectAdapter(adapter);
    const quota = svc.getQuotaStatus();
    expect(quota.usedBytes).toBe(3000);
    expect(quota.pinnedUsedBytes).toBe(1000);
  });

  it('counts a CID as pinned when ANY sidecar pins it (is_cid_pinned_by_any mirror)', async () => {
    const svc = new FileManagerService();
    const { adapter } = adapterWith({
      list_content: [
        // First-seen entry unpinned; symlink sibling pins the same CID.
        wireItem({ sidecarId: 'a', cid: 'c-shared', sizeBytes: 700, pinned: false }),
        wireItem({ sidecarId: 'b', cid: 'c-shared', sizeBytes: 700, pinned: true }),
      ],
      get_storage_budget: { cacheCapacity: 512, maxPinnedBytes: 50_000_000 },
    });
    await svc.connectAdapter(adapter);
    expect(svc.getQuotaStatus().pinnedUsedBytes).toBe(700);
  });

  it('budget fetch failure degrades to null budget (used-only display)', async () => {
    const svc = new FileManagerService();
    const { adapter } = adapterWith({
      list_content: [],
      get_storage_budget: () => Promise.reject(new Error('runtime unavailable')),
    });
    await svc.connectAdapter(adapter);
    expect(svc.getQuotaStatus().pinnedBudgetBytes).toBeNull();
  });

  it('returns cleanup recommendations sorted by confidence descending', () => {
    const svc = new FileManagerService();
    const recs = svc.getCleanupRecommendations();
    expect(recs.length).toBeGreaterThan(0);
    for (let i = 1; i < recs.length; i++) {
      expect(recs[i - 1].confidence).toBeGreaterThanOrEqual(recs[i].confidence);
    }
  });

  it('returns published content', () => {
    const svc = new FileManagerService();
    const published = svc.getPublishedContent();
    expect(published.length).toBe(3);
  });

  it('burn removes content and frees quota', () => {
    const svc = new FileManagerService();
    const before = svc.getQuotaStatus();
    const target = svc.getContents().find((i) => i.cid === 'cid-training-data');
    expect(target).toBeDefined();

    svc.burn([target!.sidecarId]);

    const after = svc.getQuotaStatus();
    expect(after.usedBytes).toBe(before.usedBytes - target!.sizeBytes);
    // Item should no longer appear in contents
    expect(svc.getContents().find((i) => i.cid === 'cid-training-data')).toBeUndefined();
  });

  it('burn filters out burned items from cleanup recommendations', () => {
    const svc = new FileManagerService();
    svc.burn(['mock-sidecar-7']);
    const recs = svc.getCleanupRecommendations();
    expect(recs.find((r) => r.cid === 'cid-training-data')).toBeUndefined();
  });

  it('pin toggles pinned state on', () => {
    const svc = new FileManagerService();
    const item = svc.getContents().find((i) => i.cid === 'cid-video-lecture');
    expect(item!.pinned).toBe(false);

    svc.pin('mock-sidecar-5');

    const updated = svc.getContents().find((i) => i.cid === 'cid-video-lecture');
    expect(updated!.pinned).toBe(true);
  });

  it('unpin toggles pinned state off', () => {
    const svc = new FileManagerService();
    // cid-song-favorite is pinned by default
    const item = svc.getContents().find((i) => i.cid === 'cid-song-favorite');
    expect(item!.pinned).toBe(true);

    svc.unpin('mock-sidecar-4');

    const updated = svc.getContents().find((i) => i.cid === 'cid-song-favorite');
    expect(updated!.pinned).toBe(false);
  });

  it('publish moves content to published as durable and removes from private', () => {
    const svc = new FileManagerService();
    const beforePrivate = svc.getContents().length;
    const beforePublished = svc.getPublishedContent().length;

    svc.publish(['cid-app-build']);

    expect(svc.getContents().length).toBe(beforePrivate - 1);
    expect(svc.getPublishedContent().length).toBe(beforePublished + 1);
    // Should not be in private anymore
    expect(svc.getContents().find((i) => i.cid === 'cid-app-build')).toBeUndefined();
    // Should be in published with durable mode
    const pub = svc.getPublishedContent().find((i) => i.cid === 'cid-app-build');
    expect(pub).toBeDefined();
    expect(pub!.publishMode).toBe('durable');
  });

  it('release moves content to published as ephemeral', () => {
    const svc = new FileManagerService();
    const beforePublished = svc.getPublishedContent().length;

    svc.release(['cid-design-doc']);

    const pub = svc.getPublishedContent().find((i) => i.cid === 'cid-design-doc');
    expect(pub).toBeDefined();
    expect(pub!.publishMode).toBe('ephemeral');
    expect(svc.getPublishedContent().length).toBe(beforePublished + 1);
    expect(svc.getContents().find((i) => i.cid === 'cid-design-doc')).toBeUndefined();
  });

  it('setReplicationTier updates tier for specified items', () => {
    const svc = new FileManagerService();
    svc.setReplicationTier(['mock-sidecar-5', 'mock-sidecar-7'], 'high');

    const video = svc.getContents().find((i) => i.cid === 'cid-video-lecture');
    const data = svc.getContents().find((i) => i.cid === 'cid-training-data');
    expect(video!.replicationTier).toBe('high');
    expect(data!.replicationTier).toBe('high');
  });

  it('getContentDetail returns only real fields (ZEB-612 S3: mock peers/origin gone)', () => {
    const svc = new FileManagerService();
    const detail = svc.getContentDetail('cid-song-favorite');
    expect(detail).toBeDefined();
    expect(detail!.cid).toBe('cid-song-favorite');
    expect('origin' in detail!).toBe(false);
    expect('sharedWith' in detail!).toBe(false);
    expect('storageBuddies' in detail!).toBe(false);
  });

  it('getContentDetail returns undefined for unknown cid', () => {
    const svc = new FileManagerService();
    expect(svc.getContentDetail('nonexistent')).toBeUndefined();
  });

  it('each instance is independent (structuredClone)', () => {
    const svc1 = new FileManagerService();
    const svc2 = new FileManagerService();
    svc1.burn(['mock-sidecar-7']);
    // svc2 should still have the item
    expect(svc2.getContents().find((i) => i.cid === 'cid-training-data')).toBeDefined();
  });

  it('archive removes content from private and frees quota', () => {
    const svc = new FileManagerService();
    const before = svc.getQuotaStatus();
    const target = svc.getContents().find((i) => i.cid === 'cid-design-doc');
    expect(target).toBeDefined();

    svc.archive([target!.sidecarId]);

    const after = svc.getQuotaStatus();
    expect(after.usedBytes).toBe(before.usedBytes - target!.sizeBytes);
    expect(svc.getContents().find((i) => i.cid === 'cid-design-doc')).toBeUndefined();
  });

  it('archive invokes archive_content on the adapter for each sidecar_id', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.archive(['mock-sidecar-2', 'mock-sidecar-5']);
    expect(adapter.invoke).toHaveBeenCalledWith('archive_content', { sidecarId: 'mock-sidecar-2' });
    expect(adapter.invoke).toHaveBeenCalledWith('archive_content', { sidecarId: 'mock-sidecar-5' });
  });

  it('exportToDevice invokes export_content with cid as filename when item not in real list', async () => {
    // After connectAdapter, the mock list_content returns no items, so the
    // service has an empty privateContent. exportToDevice falls back to the
    // CID as the filename for items not present in the real list.
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    await svc.exportToDevice(['cid-design-doc']);
    expect(adapter.invoke).toHaveBeenCalledWith('export_content', {
      cid: 'cid-design-doc',
      fileName: 'cid-design-doc',
    });
  });

  it('exportToDevice uses cid as fallback name for unknown items', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    await svc.exportToDevice(['unknown-cid']);
    expect(adapter.invoke).toHaveBeenCalledWith('export_content', {
      cid: 'unknown-cid',
      fileName: 'unknown-cid',
    });
  });

  it('exportToDevice is a no-op without adapter', async () => {
    const svc = new FileManagerService();
    // Should not throw when adapter is null
    await expect(svc.exportToDevice(['cid-design-doc'])).resolves.toBeUndefined();
  });

  // ── connectAdapter ────────────────────────────────────────────────

  it('registers a content-announced listener', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    expect(adapter.listen).toHaveBeenCalledWith('content-announced', expect.any(Function));
  });

  it('idempotent — second connectAdapter is a no-op', async () => {
    const svc = new FileManagerService();
    const { adapter: a1 } = createMockAdapter();
    const { adapter: a2 } = createMockAdapter();
    await svc.connectAdapter(a1);
    await svc.connectAdapter(a2);
    expect(a2.listen).not.toHaveBeenCalled();
  });

  it('tracks announced CIDs from network events', async () => {
    const svc = new FileManagerService();
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('content-announced', { cid: 'abc123', sizeBytes: 4096 } satisfies ContentAnnouncementEvent);
    expect(svc.announcedCids.has('abc123')).toBe(true);
    expect(svc.announcedCids.get('abc123')!.sizeBytes).toBe(4096);
  });

  it('deduplicates announced CIDs', async () => {
    const svc = new FileManagerService();
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.onChange = vi.fn(); // reset after connect so we count only post-connect calls
    emit('content-announced', { cid: 'dup1', sizeBytes: 100 } satisfies ContentAnnouncementEvent);
    emit('content-announced', { cid: 'dup1', sizeBytes: 200 } satisfies ContentAnnouncementEvent);
    expect(svc.announcedCids.get('dup1')!.sizeBytes).toBe(100); // first wins
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  it('calls onChange on new content announcement', async () => {
    const svc = new FileManagerService();
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.onChange = vi.fn(); // reset after connect so we count only post-connect calls
    emit('content-announced', { cid: 'new1', sizeBytes: 512 } satisfies ContentAnnouncementEvent);
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  // ── adapter invoke on mutations ───────────────────────────────────

  it('burn invokes burn_content on the adapter for each sidecar_id', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.burn(['mock-sidecar-7', 'mock-sidecar-5']);
    expect(adapter.invoke).toHaveBeenCalledWith('burn_content', { sidecarId: 'mock-sidecar-7' });
    expect(adapter.invoke).toHaveBeenCalledWith('burn_content', { sidecarId: 'mock-sidecar-5' });
  });

  it('pin invokes pin_content on the adapter', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.pin('mock-sidecar-5');
    expect(adapter.invoke).toHaveBeenCalledWith('pin_content', { sidecarId: 'mock-sidecar-5' });
  });

  it('unpin invokes unpin_content on the adapter', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.unpin('mock-sidecar-4');
    expect(adapter.invoke).toHaveBeenCalledWith('unpin_content', { sidecarId: 'mock-sidecar-4' });
  });

  // ── destroy / addUnlisten ─────────────────────────────────────────

  it('destroy calls all registered unlisteners', async () => {
    const svc = new FileManagerService();
    const { adapter, unlisten } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const external = vi.fn();
    svc.addUnlisten(external);
    svc.destroy();
    expect(unlisten).toHaveBeenCalledOnce();
    expect(external).toHaveBeenCalledOnce();
  });

  it('destroy is safe to call twice', async () => {
    const svc = new FileManagerService();
    const { adapter, unlisten } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.destroy();
    svc.destroy();
    expect(unlisten).toHaveBeenCalledOnce();
  });

  // ── inferCategory ────────────────────────────────────────────────

  it('inferCategory identifies music files', () => {
    expect(inferCategory('song.mp3')).toBe('music');
    expect(inferCategory('album.flac')).toBe('music');
    expect(inferCategory('track.ogg')).toBe('music');
  });

  it('inferCategory identifies video files', () => {
    expect(inferCategory('clip.mp4')).toBe('video');
    expect(inferCategory('movie.mkv')).toBe('video');
  });

  it('inferCategory identifies image files', () => {
    expect(inferCategory('photo.jpg')).toBe('image');
    expect(inferCategory('icon.png')).toBe('image');
    expect(inferCategory('diagram.svg')).toBe('image');
  });

  it('inferCategory identifies software files', () => {
    expect(inferCategory('app.exe')).toBe('software');
    expect(inferCategory('release.tar')).toBe('software');
    expect(inferCategory('bundle.zip')).toBe('software');
  });

  it('inferCategory identifies dataset files', () => {
    expect(inferCategory('data.csv')).toBe('dataset');
    expect(inferCategory('table.parquet')).toBe('dataset');
  });

  it('inferCategory falls back to text', () => {
    expect(inferCategory('readme.md')).toBe('text');
    expect(inferCategory('config.toml')).toBe('text');
    expect(inferCategory('noextension')).toBe('text');
  });

  it('inferCategory is case-insensitive', () => {
    expect(inferCategory('TRACK.MP3')).toBe('music');
    expect(inferCategory('Photo.JPG')).toBe('image');
  });

  // ── ingest ───────────────────────────────────────────────────────

  it('ingest calls ingest_content on the adapter and adds item', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'ingest_content') {
        return Promise.resolve({ cid: 'abc123', fileName: 'photo.jpg', sizeBytes: 4096 });
      }
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);

    const beforeCount = svc.getContents().length;
    const item = await svc.ingest();

    expect(adapter.invoke).toHaveBeenCalledWith('ingest_content');
    expect(item).toBeDefined();
    expect(item!.cid).toBe('abc123');
    expect(item!.name).toBe('photo.jpg');
    expect(item!.category).toBe('image');
    expect(item!.sizeBytes).toBe(4096);
    expect(item!.sensitivity).toBe('private');
    expect(item!.replicationTier).toBe('default');
    expect(svc.getContents().length).toBe(beforeCount + 1);
  });

  it('ingest assigns parentCid when provided', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockResolvedValue({ cid: 'def456', fileName: 'doc.md', sizeBytes: 100 });
    await svc.connectAdapter(adapter);

    const item = await svc.ingest('cid-folder-projects');
    expect(item!.parentCid).toBe('cid-folder-projects');
  });

  it('ingest returns undefined without adapter', async () => {
    const svc = new FileManagerService();
    const result = await svc.ingest();
    expect(result).toBeUndefined();
  });

  it('ingest updates quota', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockResolvedValue({ cid: 'ghi789', fileName: 'data.csv', sizeBytes: 50_000 });
    await svc.connectAdapter(adapter);

    const beforeQuota = svc.getQuotaStatus().usedBytes;
    await svc.ingest();
    const afterQuota = svc.getQuotaStatus().usedBytes;
    expect(afterQuota).toBe(beforeQuota + 50_000);
  });

  it('ingest deduplicates by CID', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockResolvedValue({ cid: 'dup-cid', fileName: 'file.txt', sizeBytes: 100 });
    await svc.connectAdapter(adapter);

    const first = await svc.ingest();
    expect(first).toBeDefined();
    const countAfterFirst = svc.getContents().length;

    const second = await svc.ingest();
    expect(second).toBeUndefined();
    expect(svc.getContents().length).toBe(countAfterFirst);
  });

  it('ingest does not call onChange (caller bumps version)', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockResolvedValue({ cid: 'jkl012', fileName: 'song.mp3', sizeBytes: 1000 });
    await svc.connectAdapter(adapter);
    svc.onChange = vi.fn();

    await svc.ingest();
    expect(svc.onChange).not.toHaveBeenCalled();
  });

  it('ingest routes to ingest_content_encrypted when encrypted: true is requested', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'ingest_content_encrypted') {
        return Promise.resolve({ cid: 'enc123', fileName: 'secret.txt', sizeBytes: 256 });
      }
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);

    const item = await svc.ingest(null, { encrypted: true });

    expect(adapter.invoke).toHaveBeenCalledWith('ingest_content_encrypted');
    expect(adapter.invoke).not.toHaveBeenCalledWith('ingest_content');
    expect(item).toBeDefined();
    expect(item!.cid).toBe('enc123');
  });

  // ── grants (ZEB-674) ────────────────────────────────────────────────

  it('listGrants invokes list_grants and maps the DTO array unchanged', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    const dtos: FileGrant[] = [
      { granteeAddress: 'addr1', displayName: 'Alice', grantedAt: 1_700_000_000_000 },
      { granteeAddress: 'addr2', displayName: null, grantedAt: 1_700_000_001_000 },
    ];
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_grants') return Promise.resolve(dtos);
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);

    const result = await svc.listGrants('cidX');

    expect(adapter.invoke).toHaveBeenCalledWith('list_grants', { cid: 'cidX' });
    expect(result).toEqual(dtos);
  });

  it('grantRead invokes grant_read with camelCase args', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);

    await svc.grantRead('cidX', 'addrY');

    expect(adapter.invoke).toHaveBeenCalledWith('grant_read', { cid: 'cidX', granteeAddress: 'addrY' });
  });

  it('grantRead surfaces the ineligible rejection message unchanged (matches setBackupFlag)', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'grant_read') {
        return Promise.reject(new Error('ineligible: can only share with friends'));
      }
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);

    // setBackupFlag normalizes e instanceof Error / string but does NOT
    // strip the `ineligible: ` prefix — grantRead mirrors that exactly.
    await expect(svc.grantRead('cidX', 'addrY')).rejects.toThrow(
      'ineligible: can only share with friends',
    );
  });

  it('grantRead surfaces a string (production-shape) rejection unchanged', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'grant_read') {
        return Promise.reject('ineligible: recipient\'s devices not yet reachable');
      }
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);

    await expect(svc.grantRead('cidX', 'addrY')).rejects.toThrow(
      "ineligible: recipient's devices not yet reachable",
    );
  });

  it('revokeRead invokes revoke_read with camelCase args', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);

    await svc.revokeRead('cidX', 'addrY');

    expect(adapter.invoke).toHaveBeenCalledWith('revoke_read', { cid: 'cidX', granteeAddress: 'addrY' });
  });

  it('revokeRead surfaces an Error-shaped rejection unchanged', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'revoke_read') {
        return Promise.reject(new Error('ineligible: grant not found'));
      }
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);

    await expect(svc.revokeRead('cidX', 'addrY')).rejects.toThrow('ineligible: grant not found');
  });

  it('revokeRead surfaces a string (production-shape) rejection unchanged', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'revoke_read') {
        return Promise.reject('ineligible: grant not found');
      }
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);

    await expect(svc.revokeRead('cidX', 'addrY')).rejects.toThrow('ineligible: grant not found');
  });

  // ── received grants (ZEB-723) ───────────────────────────────────────

  const mkReceived = (cid: string, receivedAt: number): ReceivedFile => ({
    cid,
    granterAddress: 'aa',
    granterDisplay: 'aa',
    fileName: 'f',
    fileSize: 1,
    mime: 'text/plain',
    receivedAt,
  });

  it('unreadReceivedCount counts files newer than lastSeen', () => {
    const files = [mkReceived('a', 100), mkReceived('b', 200), mkReceived('c', 300)];
    expect(unreadReceivedCount(files, 150)).toBe(2); // b, c
    expect(unreadReceivedCount(files, 0)).toBe(3);
    expect(unreadReceivedCount(files, 300)).toBe(0); // strictly-newer
    expect(unreadReceivedCount([], 0)).toBe(0);
    expect(unreadReceivedCount(null, 0)).toBe(0); // unresolved → no badge
  });

  it('listReceivedGrants maps the wire DTO', async () => {
    const svc = new FileManagerService();
    const { adapter } = adapterWith({
      list_content: [],
      list_received_grants: [
        {
          cid: 'ab',
          granterAddress: 'cd',
          displayName: 'Alice',
          fileName: 'q.pdf',
          fileSize: 9,
          mime: 'application/pdf',
          receivedAt: 5,
        },
      ],
    });
    await svc.connectAdapter(adapter);

    const rows = await svc.listReceivedGrants();

    expect(rows).toEqual([
      {
        cid: 'ab',
        granterAddress: 'cd',
        granterDisplay: 'Alice',
        fileName: 'q.pdf',
        fileSize: 9,
        mime: 'application/pdf',
        receivedAt: 5,
      },
    ]);
  });

  it('listReceivedGrants falls back to the address when displayName is null', async () => {
    const svc = new FileManagerService();
    const { adapter } = adapterWith({
      list_content: [],
      list_received_grants: [
        {
          cid: 'ab',
          granterAddress: 'cd',
          displayName: null,
          fileName: 'q.pdf',
          fileSize: 9,
          mime: 'application/pdf',
          receivedAt: 5,
        },
      ],
    });
    await svc.connectAdapter(adapter);

    const rows = await svc.listReceivedGrants();

    expect(rows[0].granterDisplay).toBe('cd');
  });

  it('listReceivedGrants propagates an IPC error (never swallows to [])', async () => {
    const svc = new FileManagerService();
    const { adapter } = adapterWith({
      list_content: [],
      list_received_grants: () => Promise.reject(new Error('boom')),
    });
    await svc.connectAdapter(adapter);

    await expect(svc.listReceivedGrants()).rejects.toThrow('boom');
  });

  it('listReceivedGrants returns [] without a connected adapter (demo/test)', async () => {
    const svc = new FileManagerService();
    await expect(svc.listReceivedGrants()).resolves.toEqual([]);
  });

  it('exportReceived invokes export_content with cid and fileName', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);

    await svc.exportReceived('cid-x', 'file.pdf');

    expect(adapter.invoke).toHaveBeenCalledWith('export_content', { cid: 'cid-x', fileName: 'file.pdf' });
  });

  it('exportReceived is a no-op without adapter', async () => {
    const svc = new FileManagerService();
    await expect(svc.exportReceived('cid-x', 'file.pdf')).resolves.toBeUndefined();
  });
});
